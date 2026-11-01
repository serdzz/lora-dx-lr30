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

use lora_dx_lr30::host_log;
use lora_dx_lr30::io;
use lora_dx_lr30::protocol::{
    sf_from_index, Kind, Packet, FREQ_HZ, PACKET_LEN, SF_TABLE, TX_POWER_DBM,
};
use lora_dx_lr30::radio::{
    DxLr30, BANDWIDTH, CODING_RATE, MAX_LORA_PAYLOAD, PREAMBLE_LEN,
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
    lora_dx_lr30::host_log::run_host_log(usart, tx, rx, tx_dma, rx_dma).await;
}

/// Hardware watchdog petter. Runs as a separate task so a stuck main loop
/// (panic-in-loop, deadlocked SPI, never-resolving future) eventually starves
/// the petter, the IWDG fires, and the chip reboots. 8 s timeout / 2 s pet
/// interval gives 4× safety margin during normal operation.
#[embassy_executor::task]
async fn watchdog_task(iwdg: IWDG) {
    let mut wdt = IndependentWatchdog::new(iwdg, 8_000_000);
    wdt.unleash();
    loop {
        Timer::after(Duration::from_secs(2)).await;
        wdt.pet();
    }
}

/// GPIO assignment for the relay/sensor extension — see README's wiring
/// table for why these specific pins: PA0..PA7/PA9/PA10 are claimed by
/// the radio + UART, PB11 is the status LED, PB0 is reserved for the
/// repeater's battery-divider ADC input (unused on node_b itself, which
/// `node_b.rs`'s existing `battery_mv: 0` comment notes runs off wired
/// power) — so PB1/PB2 (relay outputs) and PB5/PB6 (sensor inputs) are
/// the first free GPIO-capable pins on this BluePill.
mod io_pins {
    pub const RELAY_COUNT: usize = 2;
    pub const SENSOR_COUNT: usize = 2;
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_stm32::init(Default::default());

    info!("node_b (PONG responder) — booting, SX1262 @ 433 MHz, UART log @ 115200");
    spawner.must_spawn(host_log_task(p.USART1, p.PA9, p.PA10, p.DMA1_CH4, p.DMA1_CH5));
    spawner.must_spawn(watchdog_task(p.IWDG));
    host_log!("node_b (PONG responder) — booting, SX1262 @ 433 MHz");

    // User LED on PB11 of the DX-SMART dev-board, active-low. 3 boot blinks,
    // then toggles on every received PING — gives a visual heartbeat in the field.
    let mut led = Output::new(p.PB11, Level::High, Speed::Low);
    for _ in 0..3 {
        led.set_low();
        Timer::after(Duration::from_millis(80)).await;
        led.set_high();
        Timer::after(Duration::from_millis(80)).await;
    }

    // Relay outputs (active-high; drive the relay module's IN pin
    // directly if it's a 3.3V-logic board — add a transistor/optocoupler
    // stage for a 5V-logic relay module, not covered here). Start both
    // de-energised (Low) — a reboot/brown-out should never leave a relay
    // latched on with no host able to confirm that's intended.
    let mut relays: [Output<'_, AnyPin>; io_pins::RELAY_COUNT] = [
        Output::new(p.PB1, Level::Low, Speed::Low).degrade(),
        Output::new(p.PB2, Level::Low, Speed::Low).degrade(),
    ];

    // Digital sensor inputs (e.g. door/window reed switch, PIR). Pulled
    // up so an open-collector/dry-contact sensor reads high when idle
    // and low when triggered — invert in the MQTT bridge if your sensor
    // wiring is the other polarity.
    let sensors: [Input<'_, AnyPin>; io_pins::SENSOR_COUNT] = [
        Input::new(p.PB5, Pull::Up).degrade(),
        Input::new(p.PB6, Pull::Up).degrade(),
    ];

    let mut spi_cfg = spi::Config::default();
    spi_cfg.frequency = Hertz(8_000_000);
    let spi = Spi::new(
        p.SPI1, p.PA5, p.PA7, p.PA6, p.DMA1_CH3, p.DMA1_CH2, spi_cfg,
    );

    // See node_a.rs — embassy-stm32 0.1.0 mis-configures MISO on STM32F1; reset
    // PA6 to floating input via PAC after Spi::new returns.
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

    let mut rx_buf = [0u8; MAX_LORA_PAYLOAD];
    // Garage deployment: node_a is now pinned to SF12 (index 5), so start
    // the scan there too — cold-start sync is immediate instead of a 5s/SF
    // walk through SF7..SF11 first. The follow-SF logic below (on
    // `ping.next_sf_index`) is a no-op once synced since node_a always
    // advertises next_sf_index == sf_index == 5.
    let mut current_sf_index: u8 = 5;

    // Fast-scan on cold-start: scan the SF table at 5 s/SF (full table in
    // 30 s) until the first packet is received, then switch to 60 s dwell.
    // Motivation: after a power-bank reboot, node_b can be tens of minutes
    // out of sync with node_a's sweep. With 60 s dwell, worst-case sync
    // takes 6 × 60 s = 6 min — long enough that a low-current power-bank
    // re-cuts before the link comes up. 5 s/SF gives a full search in
    // 30 s, well under any reasonable bank's idle-cutoff window.
    let mut synced: bool = false;

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

        if let Err(e) = lora
            .prepare_for_rx(RxMode::Continuous, &mod_params, &rx_pkt_params)
            .await
        {
            warn!("prepare_for_rx err: {}", e);
            continue;
        }

        // App-level rx timeout. Two modes:
        //   - !synced (cold-start, fast-scan): 5 s per SF — full table
        //     covered in 30 s. Keeps the link find time well under any
        //     reasonable power-bank's idle-current cutoff (~30-60 s).
        //   - synced (normal operation): 60 s per SF — recovery path when
        //     the HANDOFF_TAIL window is lost or the user walks out of
        //     range temporarily. See "Reliability" in docs/architecture.md.
        //
        // The dwell switches to 60 s on the first successful packet decode
        // (where `synced` flips to true) and stays there until reset/reboot.
        let dwell = if synced {
            Duration::from_secs(60)
        } else {
            Duration::from_secs(5)
        };
        let rx_outcome = with_timeout(dwell, lora.rx(&rx_pkt_params, &mut rx_buf)).await;
        let (len, status) = match rx_outcome {
            Ok(Ok(x)) => x,
            Ok(Err(RadioError::ReceiveTimeout)) => continue,
            Ok(Err(e)) => {
                warn!("rx err: {}", e);
                continue;
            }
            Err(_elapsed) => {
                // Timeout reached without a packet — step our own SF
                // forward. In fast-scan that's 5 s; in normal mode it's
                // 60 s.
                let prev = current_sf_index;
                current_sf_index = (current_sf_index + 1) % (SF_TABLE.len() as u8);
                let secs = dwell.as_secs();
                warn!(
                    "rx silent {}s — scanning SF {} -> {}",
                    secs,
                    7 + prev,
                    7 + current_sf_index
                );
                host_log!(
                    "rx silent {}s — scanning SF {} -> {}",
                    secs,
                    7 + prev,
                    7 + current_sf_index
                );
                let _ = lora.enter_standby().await;
                continue;
            }
        };

        if (len as usize) < PACKET_LEN {
            continue;
        }

        // io::Packet (relay/sensor) traffic shares the channel — check
        // its distinct MAGIC first, same dispatch pattern repeater.rs
        // uses for ota::Packet vs protocol::Packet.
        let buf = &rx_buf[..len as usize];
        if buf.first() == Some(&io::MAGIC) {
            handle_io_packet(&mut lora, &mod_params, &mut relays, &sensors, buf).await;
            continue;
        }

        let ping = match Packet::decode(buf) {
            Some(p) => p,
            None => {
                warn!("bad magic/version");
                continue;
            }
        };
        if ping.kind as u8 != Kind::Ping as u8 {
            continue;
        }

        // First good packet after cold-start — leave fast-scan mode.
        if !synced {
            synced = true;
            info!("synced on SF{} — switching to 60s dwell", 7 + ping.sf_index);
            host_log!("synced on SF{} — switching to 60s dwell", 7 + ping.sf_index);
        }

        led.toggle();
        info!(
            "rx ping sf={} seq={} rssi={} snr={}",
            7 + ping.sf_index,
            ping.seq,
            status.rssi,
            status.snr,
        );
        host_log!(
            "rx ping sf={} seq={} rssi={} snr={}",
            7 + ping.sf_index,
            ping.seq,
            status.rssi,
            status.snr,
        );

        let pong = Packet {
            kind: Kind::Pong,
            sf_index: ping.sf_index,
            next_sf_index: ping.next_sf_index,
            seq: ping.seq,
            echo_rssi: status.rssi,
            echo_snr: status.snr as i8,
            battery_mv: 0, // node_b garage deployment runs off wired power, not a battery
        };
        let payload = pong.encode();

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

        // Pre-emptive SF switch: the initiator tells us, via `next_sf_index`,
        // what SF the *next* ping will arrive on. On all but the last ping of
        // a round this equals current_sf_index (no-op); on the last it points
        // to the next entry in the SF sweep table, and we retune so we can
        // hear the very first ping of the new round.
        //
        // Defensive bounds-check: a corrupted byte 9 that escapes the hardware
        // CRC (rare but observed in field captures — ~1/65k packets) would
        // otherwise let an out-of-range next_sf_index latch in and strand the
        // receiver. Reject anything outside [0, SF_TABLE.len()).
        if (ping.next_sf_index as usize) >= SF_TABLE.len() {
            warn!(
                "ignoring out-of-range next_sf_index={} (CRC-escaped corruption?)",
                ping.next_sf_index
            );
            host_log!(
                "ignoring out-of-range next_sf_index={}",
                ping.next_sf_index
            );
        } else if ping.next_sf_index != current_sf_index {
            info!(
                "follow SF: {} -> {}",
                7 + current_sf_index,
                7 + ping.next_sf_index
            );
            host_log!(
                "follow SF: {} -> {}",
                7 + current_sf_index,
                7 + ping.next_sf_index
            );
            current_sf_index = ping.next_sf_index;
        }
    }
}

/// Handle one decoded `io::Packet` (relay set / sensor query). Replies
/// are sent inline, same "no queue, strict request/response" shape as
/// `repeater.rs`'s `handle_ota` — there's no "forward this to someone
/// else" step for a packet already addressed to this device specifically.
///
/// `DEVICE_ID` filtering is intentionally skipped here: node_b has no
/// multi-node addressing scheme today (it's the fixed PONG responder in
/// a single node_a<->node_b pair, same as the rest of this binary), so
/// every io::Packet received is treated as addressed to this board. If
/// this ever runs on a deployment with multiple GPIO-equipped nodes,
/// add the same `target != DEVICE_ID && target != BROADCAST` guard
/// `repeater.rs::handle_ota` uses before acting on it.
async fn handle_io_packet<RK, DLY>(
    lora: &mut LoRa<RK, DLY>,
    mod_params: &lora_phy::mod_params::ModulationParams,
    relays: &mut [Output<'_, AnyPin>; io_pins::RELAY_COUNT],
    sensors: &[Input<'_, AnyPin>; io_pins::SENSOR_COUNT],
    buf: &[u8],
) where
    RK: lora_phy::mod_traits::RadioKind,
    DLY: embedded_hal_async::delay::DelayNs,
{
    let Some(pkt) = io::Packet::decode(buf) else {
        return;
    };

    match pkt {
        io::Packet::SetRelay { target, relay_index, state } => {
            let idx = relay_index as usize;
            let ok = idx < relays.len();
            if ok {
                if state {
                    relays[idx].set_high();
                } else {
                    relays[idx].set_low();
                }
                info!("relay {} set to {}", relay_index, state);
                host_log!("relay {} set to {}", relay_index, state);
            } else {
                warn!("SetRelay: out-of-range relay_index={}", relay_index);
            }
            send_io(
                lora,
                mod_params,
                &io::Packet::RelayAck { target, relay_index, state, ok },
            )
            .await;
        }
        io::Packet::SensorQuery { target } => {
            let mut mask: u8 = 0;
            for (i, sensor) in sensors.iter().enumerate() {
                if sensor.is_high() {
                    mask |= 1 << i;
                }
            }
            info!("sensor query -> mask={:02x}", mask);
            host_log!("sensor query -> mask={:02x}", mask);
            send_io(
                lora,
                mod_params,
                &io::Packet::SensorReport { target, digital_mask: mask },
            )
            .await;
        }
        // RelayAck/SensorReport are replies node_b sends, never receives.
        io::Packet::RelayAck { .. } | io::Packet::SensorReport { .. } => {}
    }
}

/// Encode and transmit one `io::Packet` reply — mirrors `repeater.rs`'s
/// `send_ota` helper.
async fn send_io<RK, DLY>(
    lora: &mut LoRa<RK, DLY>,
    mod_params: &lora_phy::mod_params::ModulationParams,
    pkt: &io::Packet,
) where
    RK: lora_phy::mod_traits::RadioKind,
    DLY: embedded_hal_async::delay::DelayNs,
{
    let mut buf = [0u8; io::MAX_PACKET_LEN];
    let n = pkt.encode(&mut buf);
    let mut tx_pkt_params = match lora.create_tx_packet_params(PREAMBLE_LEN, false, true, false, mod_params) {
        Ok(p) => p,
        Err(e) => {
            warn!("io reply: tx pkt params err: {}", e);
            return;
        }
    };
    if let Err(e) = lora
        .prepare_for_tx(mod_params, &mut tx_pkt_params, TX_POWER_DBM, &buf[..n])
        .await
    {
        warn!("io reply: prepare_for_tx err: {}", e);
        return;
    }
    if let Err(e) = lora.tx().await {
        warn!("io reply: tx err: {}", e);
    }
}
