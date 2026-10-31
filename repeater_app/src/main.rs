//! `repeater`, extended to actually receive and apply OTA firmware updates
//! — see `../bootloader/` for the swap-and-boot stub this binary stages an
//! update for. Same ping/pong forwarding (and PONG battery stamping) as
//! `firmware/src/bin/repeater.rs` (see that file's module doc comment for
//! the repeating design — `seen` ring, jittered delay, SF following — not
//! repeated here); OTA handling is `../node_b_app/`'s, which actually
//! finishes the job `repeater.rs` stops short of.
//!
//! # What's different from `repeater.rs`'s OTA handling
//! - Stages into `swap_common::layout`'s staging region (not repeater.rs's
//!   own `UPDATE_REGION_*`), via `swap_common::flash`'s raw register driver
//!   — embassy's `Flash`, built for `stm32f103c8`, only knows 64 KB and
//!   rejects/panics on staging addresses.
//! - Erases (and read-back verifies) the whole staging region on
//!   `Announce`; repeater.rs wrote onto un-erased flash.
//! - `commit_update()` writes a real [`swap_common::layout::SwapHeader`]
//!   and resets into the bootloader, which copies staging → active.
//!
//! # Linked to run from the active-app region, not 0x08000000
//! Like `node_b_app`, this binary's `memory.x` starts FLASH at
//! `0x0800_1000`: flash `../bootloader` to `0x0800_0000` once, then this
//! to `0x0800_1000`; after that, update it over LoRa with
//! `tools/ota_push.py ... repeater_app.bin --target <DEVICE_ID>`.

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
use embassy_time::{with_timeout, Delay, Duration, Instant, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;

use lora_phy::iv::GenericSx126xInterfaceVariant;
use lora_phy::mod_params::RadioError;
use lora_phy::sx126x::{Config as Sx126xConfig, Sx126x};
use lora_phy::{LoRa, RxMode};

use lora_dx_lr30_range_test::battery::BatteryMonitor;
use lora_dx_lr30_range_test::device_id::{BROADCAST, DEVICE_ID};
use lora_dx_lr30_range_test::host_log;
use lora_dx_lr30_range_test::ota;
use lora_dx_lr30_range_test::protocol::{
    sf_from_index, Kind as PingKind, Packet as PingPacket, FREQ_HZ, PACKET_LEN, SF_TABLE, TX_POWER_DBM,
};
use lora_dx_lr30_range_test::radio::{DxLr30, BANDWIDTH, CODING_RATE, MAX_LORA_PAYLOAD, PREAMBLE_LEN};
use lora_dx_lr30_range_test::telemetry;
use swap_common::layout::{
    SwapHeader, HEADER_RESERVED, PAGE_SIZE, STAGING_LEN, STAGING_OFFSET,
};

use defmt_rtt as _;
use panic_probe as _;

/// See repeater.rs's `SEEN_LEN`.
const SEEN_LEN: usize = 8;

/// How recently this repeater must have forwarded a given telemetry
/// `device_id` before it suppresses a repeat — shorter than
/// `telemetry::BROADCAST_INTERVAL_SECS` so a genuinely new reading from
/// the same device 30s later is never mistaken for a duplicate echo of
/// the one just forwarded.
const TELEMETRY_DEDUP_WINDOW: Duration = Duration::from_secs(10);

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

/// See repeater.rs's `repeat_delay_ms`.
fn repeat_delay_ms(seq_like: u16) -> u64 {
    40 + (seq_like % 60) as u64
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_stm32::init(Default::default());

    info!("repeater_app (DEVICE_ID={}, OTA target) — booting, SX1262 @ 433 MHz", DEVICE_ID);
    spawner.must_spawn(host_log_task(p.USART1, p.PA9, p.PA10, p.DMA1_CH4, p.DMA1_CH5));
    spawner.must_spawn(watchdog_task(p.IWDG));
    host_log!("repeater_app (DEVICE_ID={}) booting", DEVICE_ID);

    let mut led = Output::new(p.PB11, Level::High, Speed::Low);
    for _ in 0..3 {
        led.set_low();
        Timer::after(Duration::from_millis(80)).await;
        led.set_high();
        Timer::after(Duration::from_millis(80)).await;
    }

    let mut battery = BatteryMonitor::new(p.ADC1, p.PB0);
    let mut battery_mv: u16 = battery.read_mv().await;
    info!("battery: {} mV", battery_mv);
    host_log!("battery: {} mV", battery_mv);

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
    let mut synced = false;
    let mut seen: [(u8, u16); SEEN_LEN] = [(0xFF, 0); SEEN_LEN];
    let mut seen_next = 0usize;
    let mut ota_rx: Option<OtaReceiver> = None;
    // Dedup for FORWARDED telemetry from other devices (separate from
    // `seen`, which dedups ping/pong by (kind, seq) — telemetry has no
    // seq number, so dedup by "have I forwarded this device_id recently"
    // instead).
    let mut seen_telemetry: [(u8, Instant); SEEN_LEN] = [(0xFF, Instant::from_ticks(0)); SEEN_LEN];
    let mut seen_telemetry_next = 0usize;
    // Own battery telemetry — see telemetry.rs for why this is a separate
    // broadcast instead of relying on PONG-stamping (which a downstream
    // repeater on the same chain would overwrite before it reaches node_a).
    let mut next_telemetry_at = Instant::now();

    loop {
        if Instant::now() >= next_telemetry_at {
            next_telemetry_at = Instant::now() + Duration::from_secs(telemetry::BROADCAST_INTERVAL_SECS);
            battery_mv = battery.read_mv().await;
            let sf = sf_from_index(current_sf_index);
            if let Ok(mod_params) = lora.create_modulation_params(sf, BANDWIDTH, CODING_RATE, FREQ_HZ) {
                let tpkt = telemetry::Packet { device_id: DEVICE_ID, battery_mv };
                let payload = tpkt.encode();
                if let Ok(mut tx_pkt_params) =
                    lora.create_tx_packet_params(PREAMBLE_LEN, false, true, false, &mod_params)
                {
                    if lora
                        .prepare_for_tx(&mod_params, &mut tx_pkt_params, TX_POWER_DBM, &payload)
                        .await
                        .is_ok()
                    {
                        let _ = lora.tx().await;
                        host_log!("telemetry broadcast: device_id={} battery={} mV", DEVICE_ID, battery_mv);
                    }
                }
            }
        }

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
        let (len, _status) = match rx_outcome {
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

        // Forward a downstream repeater's telemetry broadcast verbatim —
        // this repeater's OWN telemetry is sent separately above (its
        // own DEVICE_ID, read fresh each time), this is relaying SOMEONE
        // ELSE's reading so it isn't lost in a multi-repeater chain (see
        // telemetry.rs's module doc comment for why PONG-stamping alone
        // can't carry more than one hop's reading).
        if buf.first() == Some(&telemetry::MAGIC) {
            if let Some(t) = telemetry::Packet::decode(buf) {
                if t.device_id != DEVICE_ID {
                    let already_seen = seen_telemetry
                        .iter()
                        .any(|&(id, at)| id == t.device_id && Instant::now() < at + TELEMETRY_DEDUP_WINDOW);
                    if !already_seen {
                        seen_telemetry[seen_telemetry_next] = (t.device_id, Instant::now());
                        seen_telemetry_next = (seen_telemetry_next + 1) % seen_telemetry.len();
                        Timer::after(Duration::from_millis(repeat_delay_ms(t.battery_mv))).await;
                        if let Ok(mut tx_pkt_params) =
                            lora.create_tx_packet_params(PREAMBLE_LEN, false, true, false, &mod_params)
                        {
                            if lora
                                .prepare_for_tx(&mod_params, &mut tx_pkt_params, TX_POWER_DBM, buf)
                                .await
                                .is_ok()
                            {
                                let _ = lora.tx().await;
                                host_log!(
                                    "forwarding telemetry: device_id={} battery={} mV",
                                    t.device_id,
                                    t.battery_mv
                                );
                            }
                        }
                    }
                }
            }
            continue;
        }

        if (len as usize) < PACKET_LEN {
            continue;
        }
        let Some(pkt) = PingPacket::decode(buf) else {
            continue;
        };
        if !synced {
            synced = true;
            info!("synced on SF{}", 7 + pkt.sf_index);
        }

        let kind_u8 = pkt.kind as u8;
        if seen.iter().any(|&(k, s)| k == kind_u8 && s == pkt.seq) {
            continue;
        }
        seen[seen_next] = (kind_u8, pkt.seq);
        seen_next = (seen_next + 1) % SEEN_LEN;

        led.toggle();
        let kind_str = if pkt.kind == PingKind::Ping { "ping" } else { "pong" };
        info!("repeating {} seq={} sf={}", kind_str, pkt.seq, 7 + pkt.sf_index);
        host_log!("repeating {} seq={} sf={}", kind_str, pkt.seq, 7 + pkt.sf_index);

        Timer::after(Duration::from_millis(repeat_delay_ms(pkt.seq))).await;

        // Stamp this repeater's battery onto forwarded PONGs — see
        // repeater.rs for why only PONG.
        let mut pkt = pkt;
        if pkt.kind == PingKind::Pong {
            battery_mv = battery.read_mv().await;
            pkt.battery_mv = battery_mv;
        }

        let payload = pkt.encode();
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
        }
    }
}

/// Erases the whole staging region once, on `Announce` — see node_b_app's
/// `erase_staging` for why the raw driver and the read-back check.
fn erase_staging() -> Result<(), ()> {
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
                let ok = written == data;
                if ok {
                    rx.next_expected = index + 1;
                }
                send_ota(lora, mod_params, &ota::Packet::ChunkAck { target: DEVICE_ID, index, ok }).await;
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
    write_staging(STAGING_OFFSET + SwapHeader::FIELDS_OFFSET, &header.encode_fields_bytes());
    write_staging(STAGING_OFFSET + SwapHeader::VALID_OFFSET, &SwapHeader::encode_valid_bytes());
    host_log!("OTA image staged, resetting into bootloader for swap");
    cortex_m::peripheral::SCB::sys_reset();
}
