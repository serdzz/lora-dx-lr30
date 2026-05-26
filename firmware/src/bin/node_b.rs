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
    // Start at SF7 — node_a begins each sweep at SF7 too. From there we
    // follow `ping.next_sf_index` through SF8→SF9→…→SF12→SF7 on round
    // transitions, and fall back to round-robin scanning on rx-timeout
    // (see the `Err(_elapsed)` arm below) if the handoff window is lost.
    let mut current_sf_index: u8 = 0;

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

        // 60-second app-level rx timeout. If nothing arrives — either the
        // initiator is out of range (benign, just re-arm) or the radio /
        // DIO1 has wedged silently (the IWDG won't catch this, because the
        // executor is still alive — only this future is stuck). Either way,
        // dropping out of `rx` and re-running prepare_for_rx fully re-arms
        // the SX1262 receive path, which is the cheapest recovery short of
        // a hard reset.
        let rx_outcome = with_timeout(
            Duration::from_secs(60),
            lora.rx(&rx_pkt_params, &mut rx_buf),
        )
        .await;
        let (len, status) = match rx_outcome {
            Ok(Ok(x)) => x,
            Ok(Err(RadioError::ReceiveTimeout)) => continue,
            Ok(Err(e)) => {
                warn!("rx err: {}", e);
                continue;
            }
            Err(_elapsed) => {
                // 60 s of silence: backup recovery path for the case where
                // the entire HANDOFF_TAIL window was lost and we're stuck on
                // the previous round's SF while node_a has already moved on.
                // Step our own SF forward; within at most SF_TABLE.len()
                // timeouts (≤ 6 minutes worst-case) we'll land on whatever
                // SF node_a is now transmitting.
                let prev = current_sf_index;
                current_sf_index = (current_sf_index + 1) % (SF_TABLE.len() as u8);
                warn!(
                    "rx silent 60s — scanning SF {} -> {}",
                    7 + prev,
                    7 + current_sf_index
                );
                host_log!(
                    "rx silent 60s — scanning SF {} -> {}",
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
        let ping = match Packet::decode(&rx_buf[..len as usize]) {
            Some(p) => p,
            None => {
                warn!("bad magic/version");
                continue;
            }
        };
        if ping.kind as u8 != Kind::Ping as u8 {
            continue;
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
            //current_sf_index = ping.next_sf_index;
        }
    }
}

