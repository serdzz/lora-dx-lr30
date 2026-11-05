#![no_std]
#![no_main]

use defmt::{info, warn};
use embassy_executor::Spawner;
use embassy_stm32::exti::{Channel as _, ExtiInput};
use embassy_stm32::gpio::{AnyPin, Input, Level, Output, Pull, Speed};
use embassy_stm32::peripherals::{DMA1_CH4, DMA1_CH5, PA10, PA9, USART1};
use embassy_stm32::spi::{self, Spi};
use embassy_stm32::time::Hertz;
use embassy_time::{with_timeout, Delay, Duration, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;

use lora_phy::iv::GenericSx126xInterfaceVariant;
use lora_phy::mod_params::RadioError;
use lora_phy::sx126x::{Config as Sx126xConfig, Sx126x};
use lora_phy::{LoRa, RxMode};

use lora_dx_lr30::host_log;
use lora_dx_lr30::host_log::HostCmd;
use lora_dx_lr30::io;
use lora_dx_lr30::ota;
use lora_dx_lr30::protocol::{sf_from_index, FREQ_HZ, SF_TABLE, TX_POWER_DBM};
use lora_dx_lr30::radio::{DxLr30, BANDWIDTH, CODING_RATE, MAX_LORA_PAYLOAD, PREAMBLE_LEN};
use lora_dx_lr30::telemetry;

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

    // PING/PONG SF-sweep range test retired (04.11.26) — node_a now always
    // runs as the MQTT/IO bridge (see run_io_bridge below): it only relays
    // relay/sensor frames between the host (tools/io_mqtt_bridge.py, over
    // MQTT) and LoRa, plus surfaces repeater telemetry. `EXIT` from the host
    // used to drop back to ping-sweep mode; now it just means "the radio had
    // nothing to do", so we immediately re-enter the bridge instead of
    // falling through to a dead ping loop. `OTA` still switches to the OTA
    // bridge on request — that one actually needs to be entered manually,
    // unlike IO which is now always-on.
    loop {
        if let Ok(HostCmd::Line(line)) = host_log::HOST_CMD.try_receive() {
            if line.as_slice() == b"OTA" {
                info!("entering OTA bridge mode (host command)");
                host_log!("OTA bridge mode — relaying frames to/from LoRa");
                run_ota_bridge(&mut lora).await;
                info!("left OTA bridge mode");
                host_log!("OTA bridge mode exited");
            }
        }

        info!("entering IO bridge mode (always-on, no more ping sweep)");
        host_log!("IO bridge mode — relaying relay/sensor frames to/from LoRa");
        run_io_bridge(&mut lora).await;
        info!("IO bridge exited (EXIT or radio error) — re-entering");
        host_log!("IO bridge mode exited — re-entering");
        Timer::after_millis(200).await;
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
    // Any SF: scan the whole SF_TABLE so OTA works no matter what SF the
    // far end (repeater/node_b) happens to be on — a single fixed SF (SF12)
    // meant the OTA bridge would silently never hear an updater running at
    // a different SF. Each SF gets a short RX dwell; a host OTA frame can
    // arrive on ANY of them (the host doesn't know/care which SF is "the"
    // SF — it just wants the frame sent), so TX always uses whichever SF
    // we're currently dwelling on.
    let mut sf_table_index: usize = 0;

    loop {
        let sf_index = (sf_table_index % SF_TABLE.len()) as u8;
        sf_table_index = sf_table_index.wrapping_add(1);
        let sf = sf_from_index(sf_index);
        let mod_params = match lora.create_modulation_params(sf, BANDWIDTH, CODING_RATE, FREQ_HZ) {
            Ok(p) => p,
            Err(e) => {
                warn!("OTA bridge: mod params err: {}", e);
                return;
            }
        };

        // Race "a host frame/command arrived" against "a LoRa OTA reply
        // arrived" — whichever happens first gets handled, so this bridge
        // never has to guess which side will speak next. The RX side is
        // capped at a short dwell per SF so we keep rotating through
        // SF_TABLE even while nothing is heard.
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
        let radio_fut = with_timeout(
            Duration::from_millis(400),
            lora.rx(&rx_pkt_params, &mut rx_buf),
        );

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
                    if with_timeout(Duration::from_secs(5), lora.tx()).await.is_err() {
                        warn!("OTA bridge: tx() TIMED OUT waiting for DIO1");
                    }
                }
                HostCmd::Line(_) => {} // ignore any other text line while bridging
                HostCmd::IoFrame(_) => {} // not relevant to OTA bridge mode
            },
            embassy_futures::select::Either::Second(rx_result) => match rx_result {
                Ok(Ok((len, _status))) => {
                    let buf = &rx_buf[..len as usize];
                    if buf.first() == Some(&ota::MAGIC) {
                        host_log::push_ota_frame(buf);
                    }
                    // Non-OTA traffic heard while bridging (e.g. another
                    // node's ping/pong) is silently ignored — this bridge
                    // only forwards OTA frames to the host.
                }
                Ok(Err(RadioError::ReceiveTimeout)) => {}
                Ok(Err(e)) => {
                    warn!("OTA bridge: rx err: {}", e);
                }
                Err(_timeout) => {
                    // Per-SF dwell elapsed with nothing heard — rotate to
                    // the next SF on the next loop iteration.
                    let _ = lora.enter_standby().await;
                }
            },
        }
    }
}

/// IO bridge: node_a relays `io::Packet` relay-set/sensor-query frames
/// between the host (UART, e.g. the MQTT bridge in `tools/`) and the
/// LoRa link — same "dumb relay, host drives the state machine" split
/// as [`run_ota_bridge`], just for the smaller relay/sensor protocol
/// instead of firmware chunks.
///
/// Pinned to SF12 (index 5), same reasoning as `run_ota_bridge`: shares
/// the one antenna/radio with the ping/pong sweep, so this mode and the
/// sweep never run concurrently.
///
/// Exits when the host sends a line `b"EXIT"`.
async fn run_io_bridge<RK, DLY>(lora: &mut LoRa<RK, DLY>)
where
    RK: lora_phy::mod_traits::RadioKind,
    DLY: embedded_hal_async::delay::DelayNs,
{
    let sf_index: u8 = 5;
    let sf = sf_from_index(sf_index);
    let mod_params = match lora.create_modulation_params(sf, BANDWIDTH, CODING_RATE, FREQ_HZ) {
        Ok(p) => p,
        Err(e) => {
            warn!("IO bridge: mod params err: {}", e);
            return;
        }
    };

    loop {
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
                warn!("IO bridge: rx pkt params err: {}", e);
                return;
            }
        };
        if let Err(e) = lora.prepare_for_rx(RxMode::Continuous, &mod_params, &rx_pkt_params).await {
            warn!("IO bridge: prepare_for_rx err: {}", e);
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
                HostCmd::IoFrame(frame) => {
                    let Some(_pkt) = io::Packet::decode(&frame) else {
                        warn!("IO bridge: host sent undecodable frame, dropping");
                        continue;
                    };
                    let mut tx_pkt_params =
                        match lora.create_tx_packet_params(PREAMBLE_LEN, false, true, false, &mod_params) {
                            Ok(p) => p,
                            Err(e) => {
                                warn!("IO bridge: tx pkt params err: {}", e);
                                continue;
                            }
                        };
                    if let Err(e) =
                        lora.prepare_for_tx(&mod_params, &mut tx_pkt_params, TX_POWER_DBM, &frame).await
                    {
                        warn!("IO bridge: prepare_for_tx err: {}", e);
                        continue;
                    }
                    if with_timeout(Duration::from_secs(5), lora.tx()).await.is_err() {
                        warn!("IO bridge: tx() TIMED OUT waiting for DIO1");
                    }
                }
                HostCmd::Line(_) => {} // ignore any other text line while bridging
                HostCmd::OtaFrame(_) => {} // not relevant to IO bridge mode
            },
            embassy_futures::select::Either::Second(rx_result) => match rx_result {
                Ok((len, _status)) => {
                    let buf = &rx_buf[..len as usize];
                    if buf.first() == Some(&io::MAGIC) {
                        host_log::push_io_frame(buf);
                    } else if (len as usize) >= telemetry::PACKET_LEN
                        && buf.first() == Some(&telemetry::MAGIC)
                    {
                        // Repeaters broadcast battery telemetry on a fixed
                        // timer (see telemetry.rs), independent of
                        // ping/pong activity — it keeps arriving even
                        // while this bridge has paused the sweep, so
                        // surface it the same way the main loop does
                        // (text line over host_log!) rather than letting
                        // it vanish while IO bridge mode is active.
                        if let Some(t) = telemetry::Packet::decode(buf) {
                            host_log!(
                                "telemetry: device_id={} battery={} mV",
                                t.device_id,
                                t.battery_mv
                            );
                        }
                    }
                    // Non-IO/telemetry traffic heard while bridging
                    // (ping/pong, OTA) is silently ignored — this bridge
                    // only forwards io::Packet frames and telemetry lines
                    // to the host.
                }
                Err(RadioError::ReceiveTimeout) => {}
                Err(e) => {
                    warn!("IO bridge: rx err: {}", e);
                }
            },
        }
    }
}
