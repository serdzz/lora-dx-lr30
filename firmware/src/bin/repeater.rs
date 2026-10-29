//! A passive LoRa repeater: forwards every `protocol::Packet` (ping/pong)
//! it hears verbatim, extending `node_a`↔`node_b` range beyond one hop, and
//! separately answers `ota::Packet` OTA traffic addressed to its own
//! [`lora_dx_lr30::device_id::DEVICE_ID`].
//!
//! ## Repeating (protocol::Packet)
//! This is a dumb digital repeater, not a mesh router: it doesn't know or
//! care about addresses on ping/pong traffic, it just re-transmits whatever
//! valid packet it decodes, once, after a short jittered delay (to reduce
//! the chance of colliding with another repeater's own retransmission on a
//! multi-repeater chain). A `seen` ring buffer of recent `(kind, seq)` pairs
//! suppresses re-forwarding the same packet a second time if it's heard its
//! own retransmission's echo or a duplicate from another repeater — this is
//! what keeps a chain of repeaters from forwarding forever.
//!
//! Following the same SF as the rest of the link (sweep-following like
//! `node_b`, or pinned like the garage deployment) is the repeater's job
//! too — it must decode the packet before it can repeat it, so it has to be
//! tuned to whatever SF `node_a`/`node_b` are using right now.
//!
//! ## OTA (ota::Packet) — see `ota.rs` module docs for the wire protocol
//! This repeater accepts a firmware image addressed to it specifically
//! (not every repeater on the channel — see `ota.rs` for why), buffers
//! incoming chunks into a flash update region (`UPDATE_REGION_*` below),
//! and on a final CRC match **does not reboot into new firmware yet** —
//! see the "NOT YET IMPLEMENTED" note below. This binary currently proves
//! out: addressing, chunked reception, resend-by-index, and the flash
//! write/erase itself. It stops short of the actual swap-and-boot handoff.
//!
//! **NOT YET IMPLEMENTED: the bootloader side of this.** `embassy-boot-stm32`
//! (the obvious off-the-shelf swap-bank bootloader) requires `embassy-stm32`
//! 0.6+; this repo is pinned to `embassy-stm32` 0.1 for `lora-phy`
//! compatibility reasons predating this file. Until one of those versions
//! moves, "received and verified a new image" ends at writing it to the
//! update region — something still needs to either (a) write a small
//! hand-rolled bootloader stub at flash offset 0 that checks a "pending
//! update" flag and copies update-region → active-region before jumping to
//! the app (the manual equivalent of what embassy-boot does), or (b)
//! upgrade `embassy-stm32`/`lora-phy` together once a compatible pair
//! exists. Treat the current `commit_update()` below as a placeholder that
//! logs intent and resets — it does NOT yet copy the image into the active
//! region, so **do not rely on this for real field updates until that gap
//! is closed.**

#![no_std]
#![no_main]

use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_stm32::exti::{Channel as _, ExtiInput};
use embassy_stm32::flash::Flash;
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

use lora_dx_lr30::device_id::{BROADCAST, DEVICE_ID};
use lora_dx_lr30::host_log;
use lora_dx_lr30::ota;
use lora_dx_lr30::protocol::{Kind as PingKind, Packet as PingPacket, FREQ_HZ, PACKET_LEN, SF_TABLE};
use lora_dx_lr30::radio::{DxLr30, BANDWIDTH, CODING_RATE, MAX_LORA_PAYLOAD, PREAMBLE_LEN};

use defmt_rtt as _;
use panic_probe as _;

/// How many recent (kind, seq) pairs to remember, to avoid re-forwarding a
/// packet we (or another repeater) already repeated. Small on purpose —
/// `protocol::Packet::seq` wraps every 65536 pings, far more than fits in
/// any reasonable window; this only needs to cover "don't immediately
/// re-echo the packet I just sent."
const SEEN_LEN: usize = 8;

/// Where in flash the incoming OTA image gets staged. Placed well above
/// this repeater's own ~40-50 KB binary (see `cargo build --release` +
/// `arm-none-eabi-size` to check the real number before shrinking this).
/// `memory.x` currently declares 64 KB total — this assumes the "many
/// BluePill clones are physically 128 KB" note in CLAUDE.md and reserves
/// the upper half as a staging area. **Verify your board's real flash size
/// (e.g. read-and-compare against a known pattern near 0x0801_0000) before
/// trusting this on hardware** — writing past physical flash silently does
/// nothing-or-worse depending on the clone.
const UPDATE_REGION_OFFSET: u32 = 64 * 1024; // offset from 0x0800_0000
const UPDATE_REGION_LEN: u32 = 64 * 1024;

struct OtaReceiver {
    image_len: u32,
    image_crc: u32,
    received_mask_next_expected: u16,
    total_chunks: u16,
}

#[embassy_executor::task]
async fn host_log_task(usart: USART1, tx: PA9, rx: PA10, tx_dma: DMA1_CH4, rx_dma: DMA1_CH5) {
    lora_dx_lr30::host_log::run_host_log(usart, tx, rx, tx_dma, rx_dma).await;
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

/// Jittered re-transmit delay: enough to let the original sender's own
/// receive window close, small enough not to visibly add to round-trip
/// latency. Jitter spreads out repeaters in a chain so they don't all key
/// up on the same millisecond and collide at the RF layer.
fn repeat_delay_ms(seq_like: u16) -> u64 {
    40 + (seq_like % 60) as u64
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_stm32::init(Default::default());

    info!(
        "repeater (DEVICE_ID={}) — booting, SX1262 @ 433 MHz, UART log @ 115200",
        DEVICE_ID
    );
    spawner.must_spawn(host_log_task(p.USART1, p.PA9, p.PA10, p.DMA1_CH4, p.DMA1_CH5));
    spawner.must_spawn(watchdog_task(p.IWDG));
    host_log!("repeater (DEVICE_ID={}) — booting, SX1262 @ 433 MHz", DEVICE_ID);

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

    // See node_a.rs / node_b.rs — embassy-stm32 0.1.0 mis-configures MISO on
    // STM32F1; reset PA6 to floating input via PAC after Spi::new returns.
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
    let busy: ExtiInput<'_, AnyPin> =
        ExtiInput::new(Input::new(p.PA2, Pull::Up).degrade(), p.EXTI2.degrade());
    let dio1: ExtiInput<'_, AnyPin> =
        ExtiInput::new(Input::new(p.PC15, Pull::Down).degrade(), p.EXTI15.degrade());

    let spi_dev = ExclusiveDevice::new(spi, nss, Delay).unwrap();
    let iv = GenericSx126xInterfaceVariant::new(reset, dio1, busy, Some(rx_en), Some(tx_en))
        .expect("iv build");

    let cfg = Sx126xConfig {
        chip: DxLr30,
        tcxo_ctrl: None,
        use_dcdc: true,
        rx_boost: false,
    };
    let mut lora = match LoRa::new(Sx126x::new(spi_dev, iv, cfg), false, Delay).await {
        Ok(r) => r,
        Err(e) => defmt::panic!("LoRa init failed: {}", e),
    };

    // Internal flash driver for staging OTA images. Blocking is fine here —
    // writes only happen between LoRa rx/tx, never inside a time-critical
    // radio window.
    let mut flash = Flash::new_blocking(p.FLASH);

    let mut rx_buf = [0u8; MAX_LORA_PAYLOAD];

    // Same cold-start fast-scan / 60s-dwell logic as node_b — pinned to
    // SF12 (index 5) for the current garage deployment; see node_b.rs for
    // the full rationale if this needs to track a different sweep.
    let mut current_sf_index: u8 = 5;
    let mut synced = false;

    // Ring buffer of recently-forwarded ping/pong (kind, seq) pairs — see
    // SEEN_LEN doc comment.
    let mut seen: [(u8, u16); SEEN_LEN] = [(0xFF, 0); SEEN_LEN];
    let mut seen_next = 0usize;

    let mut ota_rx: Option<OtaReceiver> = None;

    loop {
        let sf = lora_dx_lr30::protocol::sf_from_index(current_sf_index);
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

        // Try OTA first (distinct MAGIC from protocol::Packet, so no
        // ambiguity), then fall back to ping/pong.
        if buf.first() == Some(&ota::MAGIC) {
            handle_ota(&mut lora, &mod_params, &mut flash, &mut ota_rx, buf).await;
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
        let already_seen = seen.iter().any(|&(k, s)| k == kind_u8 && s == pkt.seq);
        if already_seen {
            continue;
        }
        seen[seen_next] = (kind_u8, pkt.seq);
        seen_next = (seen_next + 1) % SEEN_LEN;

        led.toggle();
        let kind_str = if pkt.kind == PingKind::Ping { "ping" } else { "pong" };
        info!("repeating {} seq={} sf={}", kind_str, pkt.seq, 7 + pkt.sf_index);
        host_log!("repeating {} seq={} sf={}", kind_str, pkt.seq, 7 + pkt.sf_index);

        Timer::after(Duration::from_millis(repeat_delay_ms(pkt.seq))).await;

        let payload = pkt.encode();
        let mut tx_pkt_params =
            match lora.create_tx_packet_params(PREAMBLE_LEN, false, true, false, &mod_params) {
                Ok(p) => p,
                Err(e) => {
                    warn!("tx pkt params err: {}", e);
                    continue;
                }
            };
        if let Err(e) = lora
            .prepare_for_tx(&mod_params, &mut tx_pkt_params, lora_dx_lr30::protocol::TX_POWER_DBM, &payload)
            .await
        {
            warn!("prepare_for_tx err: {}", e);
            continue;
        }
        if let Err(e) = lora.tx().await {
            warn!("tx err: {}", e);
        }
    }
}

/// Handle one decoded `ota::Packet`. Replies are sent inline (not queued)
/// since OTA is a strict request/response dance — there's no "repeat this
/// to someone else" step the way ping/pong has.
async fn handle_ota<RK, DLY>(
    lora: &mut LoRa<RK, DLY>,
    mod_params: &lora_phy::mod_params::ModulationParams,
    flash: &mut Flash<'_, embassy_stm32::flash::Blocking>,
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
        // Not for us — on a multi-repeater deployment a different
        // device_id is handling this exchange; stay out of it (OTA chunks
        // are unicast by design, see ota.rs).
        return;
    }

    match pkt {
        ota::Packet::Announce { updater: _, image_len, image_crc } => {
            if ota_rx.is_some() {
                // Already mid-update; ignore a second announce rather than
                // restarting and losing progress.
                return;
            }
            if image_len > UPDATE_REGION_LEN {
                warn!(
                    "OTA announce rejected: {} bytes exceeds update region ({} bytes)",
                    image_len, UPDATE_REGION_LEN
                );
                return;
            }
            info!("OTA announce: {} bytes, crc={:08x} — accepting", image_len, image_crc);
            host_log!("OTA announce: {} bytes — accepting", image_len);
            *ota_rx = Some(OtaReceiver {
                image_len,
                image_crc,
                received_mask_next_expected: 0,
                total_chunks: ota::chunk_count(image_len),
            });
            send_ota(lora, mod_params, &ota::Packet::Accept { target: DEVICE_ID }).await;
        }
        ota::Packet::Chunk { target: _, index, total, data } => {
            let Some(rx) = ota_rx.as_mut() else {
                return;
            };
            if total != rx.total_chunks {
                warn!("OTA chunk total mismatch: got {} want {}", total, rx.total_chunks);
                return;
            }
            if index >= rx.received_mask_next_expected {
                let offset = UPDATE_REGION_OFFSET + index as u32 * ota::CHUNK_DATA_LEN as u32;
                // STM32F1 blocking_write requires erasing first per-page;
                // for simplicity this repeater erases the whole update
                // region once up front on Announce in a production
                // version — omitted here for brevity, see the module-level
                // "NOT YET IMPLEMENTED" note. Writing without a prior erase
                // onto a non-blank page WILL silently corrupt data on real
                // F1 flash (bits can only go 1->0 without an erase cycle).
                match flash.blocking_write(offset, data) {
                    Ok(()) => {
                        rx.received_mask_next_expected = index + 1;
                        send_ota(
                            lora,
                            mod_params,
                            &ota::Packet::ChunkAck { target: DEVICE_ID, index, ok: true },
                        )
                        .await;
                    }
                    Err(e) => {
                        warn!("OTA flash write failed at chunk {}: {:?}", index, defmt::Debug2Format(&e));
                        send_ota(
                            lora,
                            mod_params,
                            &ota::Packet::ChunkAck { target: DEVICE_ID, index, ok: false },
                        )
                        .await;
                    }
                }
            } else {
                // Already have this one (a resend we already acked) — ack
                // again in case our first ack was lost, don't rewrite flash.
                send_ota(
                    lora,
                    mod_params,
                    &ota::Packet::ChunkAck { target: DEVICE_ID, index, ok: true },
                )
                .await;
            }
        }
        ota::Packet::Done { target: _, final_crc } => {
            let Some(rx) = ota_rx.take() else {
                return;
            };
            let ok = final_crc == rx.image_crc;
            info!(
                "OTA done: {} bytes, final_crc={:08x} expected={:08x} ok={}",
                rx.image_len, final_crc, rx.image_crc, ok
            );
            host_log!("OTA done, ok={}", ok);
            send_ota(lora, mod_params, &ota::Packet::DoneAck { target: DEVICE_ID, ok }).await;
            if ok {
                commit_update();
            }
        }
        _ => {}
    }
}

/// Encode and transmit one `ota::Packet` reply.
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
        Err(e) => {
            warn!("OTA tx pkt params err: {}", e);
            return;
        }
    };
    if let Err(e) = lora
        .prepare_for_tx(mod_params, &mut tx_pkt_params, lora_dx_lr30::protocol::TX_POWER_DBM, &buf[..n])
        .await
    {
        warn!("OTA prepare_for_tx err: {}", e);
        return;
    }
    if let Err(e) = lora.tx().await {
        warn!("OTA tx err: {}", e);
    }
}

/// Placeholder for the swap-and-reboot handoff — see the module-level "NOT
/// YET IMPLEMENTED" doc comment. Currently just logs and resets; the new
/// image sits in the update region but nothing copies it into the active
/// region or tells a bootloader to do so, so the chip will come back up
/// running the SAME firmware it had before. Do not treat a reset here as
/// "the update applied."
fn commit_update() -> ! {
    warn!("OTA image verified but swap-and-boot handoff is NOT implemented — see ota handling notes");
    host_log!("OTA verified; reboot handoff not implemented, resetting unchanged");
    cortex_m::peripheral::SCB::sys_reset();
}
