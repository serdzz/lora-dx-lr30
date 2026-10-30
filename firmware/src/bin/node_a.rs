#![no_std]
#![no_main]

use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_stm32::exti::{Channel as _, ExtiInput};
use embassy_stm32::gpio::{AnyPin, Input, Level, Output, Pull, Speed};
use embassy_stm32::peripherals::{DMA1_CH4, DMA1_CH5, PA10, PA9, USART1};
use embassy_stm32::spi::{self, Spi};
use embassy_stm32::time::Hertz;
use embassy_time::{Delay, Duration, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;

use lora_phy::iv::GenericSx126xInterfaceVariant;
use lora_phy::mod_params::RadioError;
use lora_phy::sx126x::{Config as Sx126xConfig, Sx126x};
use lora_phy::{LoRa, RxMode};

use lora_dx_lr30::host_log;
use lora_dx_lr30::host_log::HostCmd;
use lora_dx_lr30::ota;
use lora_dx_lr30::protocol::{
    sf_from_index, Kind, Packet, FREQ_HZ, PACKET_LEN, PINGS_PER_SF,
    TX_POWER_DBM,
};
use lora_dx_lr30::radio::{
    DxLr30, BANDWIDTH, CODING_RATE, MAX_LORA_PAYLOAD, PREAMBLE_LEN, RX_SYMBOL_TIMEOUT,
};

use defmt_rtt as _;
use panic_probe as _;

#[embassy_executor::task]
async fn host_log_task(
    usart: USART1,
    tx: PA9,
    rx: PA10,
    tx_dma: DMA1_CH4,
    rx_dma: DMA1_CH5,
) {
    // node_a is the one binary that reads host commands back (OTA bridge
    // mode) — see host_log.rs's module doc comment.
    lora_dx_lr30::host_log::run_host_log_and_cmd(usart, tx, rx, tx_dma, rx_dma).await;
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_stm32::init(Default::default());

    info!("node_a (PING initiator) — booting, SX1262 @ 433 MHz, UART log @ 115200");
    spawner.must_spawn(host_log_task(p.USART1, p.PA9, p.PA10, p.DMA1_CH4, p.DMA1_CH5));
    host_log!("node_a (PING initiator) — booting, SX1262 @ 433 MHz");

    // User LED on PB11 of the DX-SMART dev-board, active-low (drive low to light).
    // Field indicator: 3 quick blinks at boot, then toggles on every PONG received.
    let mut led = Output::new(p.PB11, Level::High, Speed::Low);
    for _ in 0..3 {
        led.set_low();
        Timer::after(Duration::from_millis(80)).await;
        led.set_high();
        Timer::after(Duration::from_millis(80)).await;
    }

    // SPI1 = PA5/PA7/PA6 (SCK/MOSI/MISO). 8 MHz is well within SX1262 spec (16 MHz max).
    let mut spi_cfg = spi::Config::default();
    spi_cfg.frequency = Hertz(8_000_000);
    let spi = Spi::new(
        p.SPI1, p.PA5, p.PA7, p.PA6, p.DMA1_CH3, p.DMA1_CH2, spi_cfg,
    );

    // embassy-stm32 0.1.0 bug: Spi::new() on gpio_v1 (STM32F1) calls
    // `miso.set_speed(VeryHigh)` AFTER configuring MISO as AF Input. On F1 that
    // rewrites MODE-bits to OUTPUT_50MHZ while CNF stays at 0b01 — which the
    // hardware reinterprets as GPIO open-drain OUTPUT, so PA6 is pulled to 0V
    // and we never read MISO from the SX1262. Fix: drop PA6 back to MODE=INPUT
    // + CNF=FLOATING via PAC after the SPI driver init.
    {
        use embassy_stm32::pac::gpio::vals::{CnfIn, Mode};
        embassy_stm32::pac::GPIOA.cr(0).modify(|w| {
            w.set_mode(6, Mode::INPUT);
            w.set_cnf_in(6, CnfIn::FLOATING);
        });
    }

    // NSS stays typed — only the lora-phy IV needs same-type pins.
    let nss = Output::new(p.PA4, Level::High, Speed::VeryHigh);

    // All CTRL pins must share one concrete type for GenericSx126xInterfaceVariant<CTRL, WAIT>.
    let reset: Output<'_, AnyPin> = Output::new(p.PA3, Level::High, Speed::Low).degrade();
    let rx_en: Output<'_, AnyPin> = Output::new(p.PA1, Level::Low, Speed::Low).degrade();
    let tx_en: Output<'_, AnyPin> = Output::new(p.PA0, Level::Low, Speed::Low).degrade();

    // BUSY and DIO1 must share one concrete WAIT type — degrade both to AnyPin.
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

    let mut rx_buf = [0u8; MAX_LORA_PAYLOAD];
    // Garage deployment: pinned to SF12 (index 5) — max sensitivity/range.
    // See docs/speed-vs-range.md. Confirmed via earlier sweep test that even
    // SF12 (best available) dropped the link before reaching the garage, so
    // this alone won't fix range — the real cause is likely a physical
    // obstruction, antenna fault, or power loss along the walk, not SF
    // choice. Pinned here anyway to remove SF as a variable for the next
    // field test and to avoid re-sweeping through weaker SFs first.
    let sf_index: u8 = 5;

    loop {
        // Drain any pending host command before starting the next sweep
        // round. "OTA" switches into the bridge loop below (which doesn't
        // return until the host sends "EXIT" or the LoRa side reports a
        // terminal OTA outcome) — ping/pong sweeping is paused for the
        // duration, same radio/antenna, so OTA traffic and range-test
        // traffic never run concurrently on this one node.
        if let Ok(HostCmd::Line(line)) = host_log::HOST_CMD.try_receive() {
            if line.as_slice() == b"OTA" {
                info!("entering OTA bridge mode (host command)");
                host_log!("OTA bridge mode — relaying frames to/from LoRa");
                run_ota_bridge(&mut lora).await;
                info!("left OTA bridge mode");
                host_log!("OTA bridge mode exited");
            }
        }

        let sf = sf_from_index(sf_index);
        let mod_params = match lora.create_modulation_params(sf, BANDWIDTH, CODING_RATE, FREQ_HZ) {
            Ok(p) => p,
            Err(e) => {
                warn!("mod params err: {}", e);
                continue;
            }
        };

        let mut hits: u16 = 0;
        let mut sum_rssi_local: i32 = 0;
        let mut sum_snr_local: i32 = 0;
        let mut sum_rssi_remote: i32 = 0;
        let mut sum_snr_remote: i32 = 0;

        info!("=== SF{} round start ({} pings) ===", 7 + sf_index, PINGS_PER_SF);

        for seq in 0..PINGS_PER_SF {
            // Robust SF handoff: in the LAST `HANDOFF_TAIL` pings of the round,
            // advertise the next SF. node_b switches on the FIRST one it
            // receives, so even if 2 of 3 are lost, sync survives. Earlier
            // pings keep `next_sf_index = sf_index` (no-op for node_b).
            // Garage deployment pins sf_index, so never advertise a switch:
            // advertising (sf_index + 1) here sent node_b to SF7 at seq 18
            // while node_a stayed on SF12, killing the link for good.
            let next_sf_index = sf_index;
            let pkt = Packet {
                kind: Kind::Ping,
                sf_index,
                next_sf_index,
                seq,
                echo_rssi: 0,
                echo_snr: 0,
                battery_mv: 0, // node_a is hand-held, no battery ADC wired
            };
            let payload = pkt.encode();

            let mut tx_pkt_params = match lora.create_tx_packet_params(
                PREAMBLE_LEN,
                false,
                true,
                false,
                &mod_params,
            ) {
                Ok(p) => p,
                Err(e) => {
                    warn!("tx pkt params err: {}", e);
                    continue;
                }
            };

            if let Err(e) = lora
                .prepare_for_tx(&mod_params, &mut tx_pkt_params, TX_POWER_DBM, &payload)
                .await
            {
                warn!("prepare_for_tx err: {}", e);
                continue;
            }
            if let Err(e) = lora.tx().await {
                warn!("tx err: {}", e);
                continue;
            }

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

            if let Err(e) = lora
                .prepare_for_rx(
                    RxMode::Single(RX_SYMBOL_TIMEOUT),
                    &mod_params,
                    &rx_pkt_params,
                )
                .await
            {
                warn!("prepare_for_rx err: {}", e);
                continue;
            }

            match lora.rx(&rx_pkt_params, &mut rx_buf).await {
                Ok((len, status)) => {
                    if (len as usize) < PACKET_LEN {
                        warn!("short packet len={}", len);
                        continue;
                    }
                    let resp = match Packet::decode(&rx_buf[..len as usize]) {
                        Some(p) => p,
                        None => {
                            warn!("bad magic/version");
                            continue;
                        }
                    };
                    if resp.kind as u8 != Kind::Pong as u8 || resp.seq != seq {
                        warn!("unexpected resp kind/seq: seq_want={} seq_got={}", seq, resp.seq);
                        continue;
                    }
                    hits += 1;
                    sum_rssi_local += status.rssi as i32;
                    sum_snr_local += status.snr as i32;
                    sum_rssi_remote += resp.echo_rssi as i32;
                    sum_snr_remote += resp.echo_snr as i32;
                    led.toggle();
                    info!(
                        "sf={} seq={} rx_rssi={} rx_snr={} tx_rssi={} tx_snr={}",
                        7 + sf_index,
                        seq,
                        status.rssi,
                        status.snr,
                        resp.echo_rssi,
                        resp.echo_snr,
                    );
                    host_log!(
                        "sf={} seq={} rx_rssi={} rx_snr={} tx_rssi={} tx_snr={}",
                        7 + sf_index,
                        seq,
                        status.rssi,
                        status.snr,
                        resp.echo_rssi,
                        resp.echo_snr,
                    );
                }
                Err(RadioError::ReceiveTimeout) => {
                    warn!("miss sf={} seq={}", 7 + sf_index, seq);
                    host_log!("miss sf={} seq={}", 7 + sf_index, seq);
                }
                Err(e) => {
                    warn!("rx err sf={} seq={}: {}", 7 + sf_index, seq, e);
                }
            }

            Timer::after_millis(50).await;
        }

        let total = PINGS_PER_SF as i32;
        let per_pct = ((total - hits as i32) * 100) / total;
        if hits > 0 {
            let h = hits as i32;
            info!(
                "=== SF{} summary: hits={}/{} per={}% rx_rssi_avg={} rx_snr_avg={} tx_rssi_avg={} tx_snr_avg={} ===",
                7 + sf_index, hits, PINGS_PER_SF, per_pct,
                sum_rssi_local / h, sum_snr_local / h,
                sum_rssi_remote / h, sum_snr_remote / h,
            );
            host_log!(
                "=== SF{} summary: hits={}/{} per={}% rx_rssi_avg={} rx_snr_avg={} tx_rssi_avg={} tx_snr_avg={} ===",
                7 + sf_index, hits, PINGS_PER_SF, per_pct,
                sum_rssi_local / h, sum_snr_local / h,
                sum_rssi_remote / h, sum_snr_remote / h,
            );
        } else {
            info!(
                "=== SF{} summary: hits=0/{} per=100% (no link) ===",
                7 + sf_index, PINGS_PER_SF
            );
            host_log!(
                "=== SF{} summary: hits=0/{} per=100% (no link) ===",
                7 + sf_index, PINGS_PER_SF
            );
        }

        // Garage deployment: stay pinned on sf_index (SF10) — no sweep advance.
        Timer::after_millis(500).await;
    }
}

/// OTA bridge: node_a becomes a dumb relay between the host (via UART,
/// `tools/ota_push.py`) and the LoRa link, while an update is pushed to a
/// target device (`node_b_app` or `repeater`). node_a's own `DEVICE_ID` is
/// irrelevant here — it never terminates an OTA exchange itself, it just
/// transmits whatever `ota::Packet` frame the host sends and relays
/// whatever OTA reply comes back over the air, so the host-side script
/// drives the actual state machine (Announce → wait Accept → send Chunks
/// → wait Done → DoneAck) and can show progress/retry logic in Python
/// instead of firmware.
///
/// Pinned to SF12 (matches the garage deployment's `sf_index` — same
/// reasoning as the main loop: this bridge runs on the same antenna/link
/// as the ping/pong sweep, no separate radio).
///
/// Exits when the host sends a line `b"EXIT"`.
async fn run_ota_bridge<RK, DLY>(lora: &mut LoRa<RK, DLY>)
where
    RK: lora_phy::mod_traits::RadioKind,
    DLY: embedded_hal_async::delay::DelayNs,
{
    let sf_index: u8 = 5;
    let sf = sf_from_index(sf_index);
    let mod_params = match lora.create_modulation_params(sf, BANDWIDTH, CODING_RATE, FREQ_HZ) {
        Ok(p) => p,
        Err(e) => {
            warn!("OTA bridge: mod params err: {}", e);
            return;
        }
    };

    loop {
        // Race "a host frame/command arrived" against "a LoRa OTA reply
        // arrived" — whichever happens first gets handled, so this bridge
        // never has to guess which side will speak next.
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
                warn!("OTA bridge: rx pkt params err: {}", e);
                return;
            }
        };
        if let Err(e) = lora.prepare_for_rx(RxMode::Continuous, &mod_params, &rx_pkt_params).await {
            warn!("OTA bridge: prepare_for_rx err: {}", e);
            return;
        }

        let mut rx_buf = [0u8; MAX_LORA_PAYLOAD];
        let host_fut = host_log::HOST_CMD.receive();
        let radio_fut = lora.rx(&rx_pkt_params, &mut rx_buf);

        match embassy_futures::select::select(host_fut, radio_fut).await {
            embassy_futures::select::Either::First(cmd) => match cmd {
                HostCmd::Line(line) if line.as_slice() == b"EXIT" => {
                    let _ = lora.enter_standby().await;
                    return;
                }
                HostCmd::OtaFrame(frame) => {
                    let Some(_pkt) = ota::Packet::decode(&frame) else {
                        warn!("OTA bridge: host sent undecodable frame, dropping");
                        continue;
                    };
                    let mut tx_pkt_params =
                        match lora.create_tx_packet_params(PREAMBLE_LEN, false, true, false, &mod_params) {
                            Ok(p) => p,
                            Err(e) => {
                                warn!("OTA bridge: tx pkt params err: {}", e);
                                continue;
                            }
                        };
                    if let Err(e) =
                        lora.prepare_for_tx(&mod_params, &mut tx_pkt_params, TX_POWER_DBM, &frame).await
                    {
                        warn!("OTA bridge: prepare_for_tx err: {}", e);
                        continue;
                    }
                    if let Err(e) = lora.tx().await {
                        warn!("OTA bridge: tx err: {}", e);
                    }
                }
                HostCmd::Line(_) => {} // ignore any other text line while bridging
            },
            embassy_futures::select::Either::Second(rx_result) => match rx_result {
                Ok((len, _status)) => {
                    let buf = &rx_buf[..len as usize];
                    if buf.first() == Some(&ota::MAGIC) {
                        host_log::push_ota_frame(buf);
                    }
                    // Non-OTA traffic heard while bridging (e.g. another
                    // node's ping/pong) is silently ignored — this bridge
                    // only forwards OTA frames to the host.
                }
                Err(RadioError::ReceiveTimeout) => {}
                Err(e) => {
                    warn!("OTA bridge: rx err: {}", e);
                }
            },
        }
    }
}
