//! `node_b`, extended to actually receive and apply OTA firmware updates
//! — see `../bootloader/` for the swap-and-boot stub this binary stages
//! an update for. Same ping/pong responder behaviour as
//! `firmware/src/bin/node_b.rs`; adds `ota::Packet` handling almost
//! verbatim ported from `firmware/src/bin/repeater.rs`'s `handle_ota` /
//! `send_ota` (see that file's module doc comment for the OTA design
//! rationale — addressing, unicast chunking, etc. — not repeated here).
//!
//! # What's different from `repeater.rs`'s OTA handling
//! `repeater.rs` stages chunks into a flash region and calls
//! `commit_update()`, which (by its own documented admission) only logs
//! intent and resets — no actual swap. This binary's `commit_update()`
//! instead writes a real [`swap_common::layout::SwapHeader`] (image_len +
//! image_crc + the `valid` marker, in that order — see the layout crate's
//! doc comment for why the ordering matters for crash-safety) into the
//! staging region, THEN resets. `../bootloader` picks that header up on
//! the next boot and does the actual active-region copy.
//!
//! # Linked to run from the active-app region, not 0x08000000
//! This binary's `memory.x` starts FLASH at `0x0800_1000` (see that
//! file's comment) — the bootloader (flashed once, separately, to
//! `0x0800_0000`) owns the real reset vector and jumps here. Flashing
//! `node_b_app` directly via `probe-rs run` for day-to-day iteration
//! (without the bootloader present) will NOT boot correctly on a fresh
//! chip — see the README for the one-time bootloader-then-app flash
//! order.

#![no_std]
#![no_main]

use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_stm32::exti::{Channel as _, ExtiInput};
use embassy_stm32::gpio::{AnyPin, Input, Level, Output, Pull, Speed};
use embassy_stm32::peripherals::{DMA1_CH4, DMA1_CH5, IWDG, PA10, PA9, USART1};
use embassy_stm32::spi::{self, Spi};
use embassy_stm32::time::Hertz;
use embassy_stm32::wdg::IndependentWatchdog;
use embassy_time::{with_timeout, Delay, Duration, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;

use lora_phy::iv::GenericSx126xInterfaceVariant;
use lora_phy::mod_params::RadioError;
use lora_phy::sx126x::{Config as Sx126xConfig, Sx126x};
use lora_phy::{LoRa, RxMode};

use lora_dx_lr30_range_test::host_log;
use lora_dx_lr30_range_test::ota;
use lora_dx_lr30_range_test::protocol::{
    sf_from_index, Kind, Packet, FREQ_HZ, PACKET_LEN, SF_TABLE, TX_POWER_DBM,
};
use lora_dx_lr30_range_test::radio::{DxLr30, BANDWIDTH, CODING_RATE, MAX_LORA_PAYLOAD, PREAMBLE_LEN};
use swap_common::layout::{
    SwapHeader, HEADER_RESERVED, PAGE_SIZE, STAGING_LEN, STAGING_OFFSET,
};

use defmt_rtt as _;
use panic_probe as _;

#[embassy_executor::task]
async fn host_log_task(usart: USART1, tx: PA9, rx: PA10, tx_dma: DMA1_CH4, rx_dma: DMA1_CH5) {
    lora_dx_lr30_range_test::host_log::run_host_log(usart, tx, rx, tx_dma, rx_dma).await;
}

#[embassy_executor::task]
async fn watchdog_task(iwdg: IWDG) {
    let mut wdt = IndependentWatchdog::new(iwdg, 8_000_000);
    wdt.unleash();
    loop {
        Timer::after(Duration::from_secs(2)).await;
        wdt.pet();
    }
}

struct OtaReceiver {
    image_len: u32,
    image_crc: u32,
    next_expected: u16,
    total_chunks: u16,
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_stm32::init(Default::default());

    info!("node_b_app (PONG responder + OTA target) — booting, SX1262 @ 433 MHz");
    spawner.must_spawn(host_log_task(p.USART1, p.PA9, p.PA10, p.DMA1_CH4, p.DMA1_CH5));
    spawner.must_spawn(watchdog_task(p.IWDG));
    host_log!("node_b_app booting");

    let mut led = Output::new(p.PB11, Level::High, Speed::Low);
    for _ in 0..3 {
        led.set_low();
        Timer::after(Duration::from_millis(80)).await;
        led.set_high();
        Timer::after(Duration::from_millis(80)).await;
    }

    let mut spi_cfg = spi::Config::default();
    spi_cfg.frequency = Hertz(8_000_000);
    let spi = Spi::new(p.SPI1, p.PA5, p.PA7, p.PA6, p.DMA1_CH3, p.DMA1_CH2, spi_cfg);

    {
        use embassy_stm32::pac::gpio::vals::{CnfIn, Mode};
        embassy_stm32::pac::GPIOA.cr(0).modify(|w| {
            w.set_mode(6, Mode::INPUT);
            w.set_cnf_in(6, CnfIn::FLOATING);
        });
    }

    let nss = Output::new(p.PA4, Level::High, Speed::VeryHigh);
    let reset: Output<'_, AnyPin> = Output::new(p.PA3, Level::High, Speed::Low).degrade();
    let rx_en: Output<'_, AnyPin> = Output::new(p.PA1, Level::Low, Speed::Low).degrade();
    let tx_en: Output<'_, AnyPin> = Output::new(p.PA0, Level::Low, Speed::Low).degrade();
    let busy: ExtiInput<'_, AnyPin> = ExtiInput::new(Input::new(p.PA2, Pull::Up).degrade(), p.EXTI2.degrade());
    let dio1: ExtiInput<'_, AnyPin> =
        ExtiInput::new(Input::new(p.PC15, Pull::Down).degrade(), p.EXTI15.degrade());

    let spi_dev = ExclusiveDevice::new(spi, nss, Delay).unwrap();
    let iv = GenericSx126xInterfaceVariant::new(reset, dio1, busy, Some(rx_en), Some(tx_en)).expect("iv build");

    let cfg = Sx126xConfig { chip: DxLr30, tcxo_ctrl: None, use_dcdc: true, rx_boost: false };
    let mut lora = match LoRa::new(Sx126x::new(spi_dev, iv, cfg), false, Delay).await {
        Ok(r) => r,
        Err(e) => defmt::panic!("LoRa init failed: {}", e),
    };

    let mut rx_buf = [0u8; MAX_LORA_PAYLOAD];
    let mut current_sf_index: u8 = 5;
    let mut synced: bool = false;
    let mut ota_rx: Option<OtaReceiver> = None;

    loop {
        let sf = sf_from_index(current_sf_index);
        let mod_params = match lora.create_modulation_params(sf, BANDWIDTH, CODING_RATE, FREQ_HZ) {
            Ok(p) => p,
            Err(e) => {
                warn!("mod params err: {}", e);
                continue;
            }
        };
        let rx_pkt_params = match lora.create_rx_packet_params(
            PREAMBLE_LEN,
            false,
            MAX_LORA_PAYLOAD as u8,
            true,
            false,
            &mod_params,
        ) {
            Ok(p) => p,
            Err(e) => {
                warn!("rx pkt params err: {}", e);
                continue;
            }
        };
        if let Err(e) = lora.prepare_for_rx(RxMode::Continuous, &mod_params, &rx_pkt_params).await {
            warn!("prepare_for_rx err: {}", e);
            continue;
        }

        let dwell = if synced { Duration::from_secs(60) } else { Duration::from_secs(5) };
        let rx_outcome = with_timeout(dwell, lora.rx(&rx_pkt_params, &mut rx_buf)).await;
        let (len, status) = match rx_outcome {
            Ok(Ok(x)) => x,
            Ok(Err(RadioError::ReceiveTimeout)) => continue,
            Ok(Err(e)) => {
                warn!("rx err: {}", e);
                continue;
            }
            Err(_elapsed) => {
                current_sf_index = (current_sf_index + 1) % (SF_TABLE.len() as u8);
                let _ = lora.enter_standby().await;
                continue;
            }
        };
        let buf = &rx_buf[..len as usize];

        if buf.first() == Some(&ota::MAGIC) {
            handle_ota(&mut lora, &mod_params, &mut ota_rx, buf).await;
            continue;
        }

        if (len as usize) < PACKET_LEN {
            continue;
        }
        let ping = match Packet::decode(buf) {
            Some(p) => p,
            None => continue,
        };
        if ping.kind as u8 != Kind::Ping as u8 {
            continue;
        }
        if !synced {
            synced = true;
            info!("synced on SF{}", 7 + ping.sf_index);
        }

        led.toggle();
        info!("rx ping sf={} seq={} rssi={} snr={}", 7 + ping.sf_index, ping.seq, status.rssi, status.snr);

        let pong = Packet {
            kind: Kind::Pong,
            sf_index: ping.sf_index,
            next_sf_index: ping.next_sf_index,
            seq: ping.seq,
            echo_rssi: status.rssi,
            echo_snr: status.snr as i8,
            battery_mv: 0,
        };
        let payload = pong.encode();
        let mut tx_pkt_params =
            match lora.create_tx_packet_params(PREAMBLE_LEN, false, true, false, &mod_params) {
                Ok(p) => p,
                Err(e) => {
                    warn!("tx pkt params err: {}", e);
                    continue;
                }
            };
        if let Err(e) = lora.prepare_for_tx(&mod_params, &mut tx_pkt_params, TX_POWER_DBM, &payload).await {
            warn!("prepare_for_tx err: {}", e);
            continue;
        }
        if let Err(e) = lora.tx().await {
            warn!("tx err: {}", e);
            continue;
        }

        if (ping.next_sf_index as usize) >= SF_TABLE.len() {
            warn!("ignoring out-of-range next_sf_index={}", ping.next_sf_index);
        } else if ping.next_sf_index != current_sf_index {
            info!("follow SF: {} -> {}", 7 + current_sf_index, 7 + ping.next_sf_index);
            current_sf_index = ping.next_sf_index;
        }
    }
}

/// Erases the whole staging region once, on `Announce` — required before
/// the first `Chunk` write; STM32F1 flash bits only go 1→0 without an
/// erase cycle (see swap-common's layout doc comment and
/// firmware/CLAUDE.md's "gotchas" section — this was a known-missing
/// piece in `repeater.rs`'s version, fixed here).
fn erase_staging() -> Result<(), ()> {
    // Staging sits past 64 KB, where embassy's `Flash` (built for
    // stm32f103c8) panics — use swap_common's raw register driver instead,
    // and only after confirming upper flash is real (not mirrored), then
    // verify the erase actually took by reading staging back as all-0xFF.
    if !swap_common::layout::upper_flash_present() {
        return Err(());
    }
    let base = swap_common::layout::FLASH_BASE + STAGING_OFFSET;
    for i in 0..STAGING_LEN / PAGE_SIZE {
        unsafe { swap_common::flash::erase_page(base + i * PAGE_SIZE) };
    }
    let staging = unsafe { core::slice::from_raw_parts(base as *const u8, STAGING_LEN as usize) };
    if staging.iter().any(|&b| b != 0xFF) {
        return Err(());
    }
    Ok(())
}

/// Writes `data` at `offset` from FLASH_BASE via the raw driver, padding an
/// odd trailing byte with 0xFF (F1 programs whole half-words only).
fn write_staging(offset: u32, data: &[u8]) {
    let addr = swap_common::layout::FLASH_BASE + offset;
    let even = data.len() & !1;
    unsafe { swap_common::flash::write(addr, &data[..even]) };
    if even < data.len() {
        unsafe { swap_common::flash::write(addr + even as u32, &[data[even], 0xFF]) };
    }
}

async fn handle_ota<RK, DLY>(
    lora: &mut LoRa<RK, DLY>,
    mod_params: &lora_phy::mod_params::ModulationParams,
    ota_rx: &mut Option<OtaReceiver>,
    buf: &[u8],
) where
    RK: lora_phy::mod_traits::RadioKind,
    DLY: embedded_hal_async::delay::DelayNs,
{
    use lora_dx_lr30_range_test::device_id::{BROADCAST, DEVICE_ID};

    let Some(pkt) = ota::Packet::decode(buf) else {
        return;
    };
    let target = pkt.addressed_to();
    if target != DEVICE_ID && target != BROADCAST {
        return;
    }

    match pkt {
        ota::Packet::Announce { updater: _, image_len, image_crc } => {
            if ota_rx.is_some() {
                return;
            }
            if image_len > STAGING_LEN - HEADER_RESERVED {
                warn!("OTA announce rejected: {} bytes too big for staging", image_len);
                return;
            }
            info!("OTA announce: {} bytes, crc={:08x}", image_len, image_crc);
            host_log!("OTA announce: {} bytes — erasing staging", image_len);
            if erase_staging().is_err() {
                warn!("staging erase failed, rejecting update");
                host_log!("OTA rejected: upper 64 KB flash missing or erase failed");
                return;
            }
            *ota_rx = Some(OtaReceiver {
                image_len,
                image_crc,
                next_expected: 0,
                total_chunks: ota::chunk_count(image_len),
            });
            send_ota(lora, mod_params, &ota::Packet::Accept { target: DEVICE_ID }).await;
        }
        ota::Packet::Chunk { target: _, index, total, data } => {
            let Some(rx) = ota_rx.as_mut() else { return };
            if total != rx.total_chunks {
                warn!("OTA chunk total mismatch");
                return;
            }
            if index >= rx.next_expected {
                let offset = STAGING_OFFSET + HEADER_RESERVED + index as u32 * ota::CHUNK_DATA_LEN as u32;
                write_staging(offset, data);
                let written = unsafe {
                    core::slice::from_raw_parts((swap_common::layout::FLASH_BASE + offset) as *const u8, data.len())
                };
                match written == data {
                    true => {
                        rx.next_expected = index + 1;
                        send_ota(lora, mod_params, &ota::Packet::ChunkAck { target: DEVICE_ID, index, ok: true })
                            .await;
                    }
                    false => {
                        send_ota(lora, mod_params, &ota::Packet::ChunkAck { target: DEVICE_ID, index, ok: false })
                            .await;
                    }
                }
            } else {
                send_ota(lora, mod_params, &ota::Packet::ChunkAck { target: DEVICE_ID, index, ok: true }).await;
            }
        }
        ota::Packet::Done { target: _, final_crc } => {
            let Some(rx) = ota_rx.take() else { return };
            let ok = final_crc == rx.image_crc;
            info!("OTA done: ok={}", ok);
            host_log!("OTA done, ok={}", ok);
            send_ota(lora, mod_params, &ota::Packet::DoneAck { target: DEVICE_ID, ok }).await;
            if ok {
                commit_update(rx.image_len, rx.image_crc);
            }
        }
        _ => {}
    }
}

async fn send_ota<RK, DLY>(
    lora: &mut LoRa<RK, DLY>,
    mod_params: &lora_phy::mod_params::ModulationParams,
    pkt: &ota::Packet<'_>,
) where
    RK: lora_phy::mod_traits::RadioKind,
    DLY: embedded_hal_async::delay::DelayNs,
{
    let mut buf = [0u8; ota::MAX_PACKET_LEN];
    let n = pkt.encode(&mut buf);
    let mut tx_pkt_params = match lora.create_tx_packet_params(PREAMBLE_LEN, false, true, false, mod_params) {
        Ok(p) => p,
        Err(_) => return,
    };
    if lora.prepare_for_tx(mod_params, &mut tx_pkt_params, TX_POWER_DBM, &buf[..n]).await.is_err() {
        return;
    }
    let _ = lora.tx().await;
}

/// Writes the swap header into staging — fields first, `valid` marker
/// last, per `swap_common::layout::SwapHeader`'s crash-safety ordering —
/// then resets into the bootloader, which performs the actual copy.
fn commit_update(image_len: u32, image_crc: u32) {
    let header = SwapHeader { image_len, image_crc };
    let fields_offset = STAGING_OFFSET + SwapHeader::FIELDS_OFFSET;
    let valid_offset = STAGING_OFFSET + SwapHeader::VALID_OFFSET;

    write_staging(fields_offset, &header.encode_fields_bytes());
    write_staging(valid_offset, &SwapHeader::encode_valid_bytes());
    host_log!("OTA image staged, resetting into bootloader for swap");
    cortex_m::peripheral::SCB::sys_reset();
}
