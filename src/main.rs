#![no_std]
#![no_main]

use bitbang_dap::{BitbangAdapter, DelayCycles, InputOutputPin};
use dap_rs::dap::{self, Dap, DapLeds, DapVersion, DelayNs};
use dap_rs::jtag::TapConfig;
use dap_rs::swo::Swo;
use defmt::{todo, unwrap, warn};
use embassy_executor::Spawner;
use embassy_futures::join::join3;
use embassy_rp::adc::{self, Adc};
use embassy_rp::flash::Flash;
use embassy_rp::gpio::{Flex, Input, Level, Output, Pin, Pull};
use embassy_rp::peripherals::{PIN_0, PIN_5, PWM_SLICE0, PWM_SLICE2, USB};
use embassy_rp::pwm::{self, Pwm, SetDutyCycle};
use embassy_rp::usb::{self, Driver as UsbDriver};
use embassy_rp::{Peri, bind_interrupts};
use embassy_time::{Duration, Ticker};
use embassy_usb::class::cdc_acm::CdcAcmClass;
use embassy_usb::class::cdc_acm::State;
use embassy_usb::class::cmsis_dap_v2::{CmsisDapV2Class, State as CmsisDapV2State};
use embassy_usb::msos::windows_version;
use embassy_usb::{self, Builder};
use heapless::String;
use static_cell::{ConstStaticCell, StaticCell};

use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => usb::InterruptHandler<USB>;
});
#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // Configuration options:
    // 1. Pinout
    let t_nrst = p.PIN_9;
    let t_jtdi = p.PIN_17;
    let t_jtms_swdio = p.PIN_10;
    let dir_swdio = p.PIN_12;
    let t_jtck_swclk = p.PIN_11;
    let dir_swclk = p.PIN_19;
    let t_jtdo = p.PIN_16;
    //let t_swo // Not supported yet

    let mut target_vcc = TargetVccReader {
        pin: adc::Channel::new_pin(p.PIN_26, Pull::None),
        adc: Adc::new_blocking(p.ADC, adc::Config::default()),
    };

    let mut target_physically_connected = TargetPhysicallyConnected {
        pin: Input::new(p.PIN_8, Pull::Up),
    };

    let mut translator_power = TranslatorPower::new(p.PIN_5, p.PWM_SLICE2);
    translator_power.set_translator_vcc(1800);

    let mut target_power = TargetPower::new(
        Output::new(p.PIN_3, Level::Low),
        // Always enable the protected 5v (existed until rev E of hardware)
        Output::new(p.PIN_7, Level::High),
        Output::new(p.PIN_6, Level::Low),
        p.PIN_0,
        p.PWM_SLICE0,
    );
    target_power.set_vtgt(1800);

    // 2. Max JTAG scan chain
    const MAX_SCAN_CHAIN_LENGTH: usize = 8;

    // 3. USB configuration
    const MANUFACTURER: &str = "me";
    const PRODUCT: &str = "Rusty Probe with Embassy CMSIS-DAP";

    // Create the driver, from the HAL.
    let driver = UsbDriver::new(p.USB, Irqs);

    // Get unique id from flash
    let mut flash = Flash::<_, _, 0>::new_blocking(p.FLASH);

    let mut uid = [0; 8];
    flash.blocking_unique_id(&mut uid).unwrap();

    static SERIAL: ConstStaticCell<String<16>> = ConstStaticCell::new(String::<16>::new());
    let serial = SERIAL.take();
    for b in uid {
        let lower = b & 0x0F;
        let upper = (b >> 4) & 0x0F;
        fn hex(nibble: u8) -> char {
            if nibble < 10 {
                (b'0' + nibble) as char
            } else {
                (b'A' + nibble - 10) as char
            }
        }
        unwrap!(serial.push(hex(upper)));
        unwrap!(serial.push(hex(lower)));
    }

    // Create embassy-usb Config
    let mut config = embassy_usb::Config::new(0xc0de, 0xcafe);
    config.manufacturer = Some(MANUFACTURER);
    config.product = Some(PRODUCT);
    config.serial_number = Some(serial);
    config.max_power = 100;
    config.max_packet_size_0 = 64;
    config.device_class = 0xEF;
    config.device_sub_class = 0x02;
    config.device_protocol = 0x01;
    config.composite_with_iads = true;

    // Create embassy-usb DeviceBuilder using the driver and config.
    // It needs some buffers for building the descriptors.
    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static MSOS_DESC: StaticCell<[u8; 196]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 128]> = StaticCell::new();
    let mut builder = Builder::new(
        driver,
        config,
        &mut CONFIG_DESC.init([0; 256])[..],
        &mut BOS_DESC.init([0; 256])[..],
        &mut MSOS_DESC.init([0; 196])[..],
        &mut CONTROL_BUF.init([0; 128])[..],
    );

    builder.msos_descriptor(windows_version::WIN8_1, 0);

    // DAP - Custom Class 0
    static DAP_STATE: ConstStaticCell<CmsisDapV2State> =
        ConstStaticCell::new(CmsisDapV2State::new());
    let mut dap_class = CmsisDapV2Class::new(&mut builder, DAP_STATE.take(), 64, true);

    // CDC - dummy class to get things working for now. Windows needs more than one interface
    // to load usbccgp.sys, which is necessary for nusb to be able to list interfaces.
    static CDC_STATE: ConstStaticCell<State> = ConstStaticCell::new(State::new());
    _ = CdcAcmClass::new(&mut builder, CDC_STATE.take(), 64);

    // Build the builder.
    let mut usb = builder.build();

    // Run the USB device.
    let usb_fut = usb.run();

    // Now create the CMSIS-DAP handler.

    static SCAN_CHAIN: ConstStaticCell<[TapConfig; MAX_SCAN_CHAIN_LENGTH]> =
        ConstStaticCell::new([TapConfig::INIT; MAX_SCAN_CHAIN_LENGTH]);
    let deps = BitbangAdapter::new(
        IoPin::new(t_nrst, None::<Peri<'static, PIN_0>>),
        IoPin::new(t_jtdi, None::<Peri<'static, PIN_0>>),
        IoPin::new(t_jtms_swdio, Some(dir_swdio)),
        IoPin::new(t_jtck_swclk, Some(dir_swclk)),
        IoPin::new(t_jtdo, None::<Peri<'static, PIN_0>>),
        BitDelay,
        SCAN_CHAIN.take(),
    );
    let mut dap = Dap::new(
        deps,
        Leds {
            _red: Output::new(p.PIN_28, Level::High),
            green: Output::new(p.PIN_27, Level::High),
            blue: Output::new(p.PIN_29, Level::High),
        },
        BitDelay,
        None::<NoSwo>,
        concat!("2.1.0, Adaptor version ", env!("CARGO_PKG_VERSION")),
    );

    let dap_fut = async {
        let mut req = [0u8; 1024];
        let mut resp = [0u8; 1024];
        loop {
            dap_class.wait_connection().await;

            let Ok(req_len) = dap_class.read_packet(&mut req).await.inspect_err(|e| {
                warn!("failed to read from USB: {:?}", e);
            }) else {
                continue;
            };

            let resp_len = dap.process_command(&req[..req_len], &mut resp, DapVersion::V2);

            if let Err(e) = dap_class.write_packet(&resp[..resp_len]).await {
                warn!("failed to write to USB: {:?}", e);
                continue;
            }
        }
    };

    let voltage_control_fut = async {
        let mut ticker = Ticker::every(Duration::from_millis(100));
        loop {
            // Set the voltage translators to track Target's VCC
            let target_vcc_mv = target_vcc.read_voltage_mv();

            if target_vcc_mv > 1500 {
                translator_power.set_translator_vcc(target_vcc_mv);
            } else {
                if target_physically_connected.target_detected() {
                    // If there is no VCC detected use 3.3v.
                    translator_power.set_translator_vcc(3300);
                } else {
                    translator_power.set_translator_vcc(0);
                }
            }

            defmt::trace!("Tracking Target VCC at {} mV", target_vcc_mv);

            ticker.next().await;
        }
    };

    // Run everything concurrently.
    // If we had made everything `'static` above instead, we could do this using separate tasks instead.
    join3(usb_fut, dap_fut, voltage_control_fut).await;
}

struct BitDelay;

impl DelayNs for BitDelay {
    fn delay_ns(&mut self, ns: u32) {
        self.delay_cycles((ns as u64 * self.cpu_clock() as u64 / 1_000_000_000_u64) as u32);
    }
}

impl DelayCycles for BitDelay {
    fn delay_cycles(&mut self, cycles: u32) {
        cortex_m::asm::delay(cycles);
    }

    fn cpu_clock(&self) -> u32 {
        // This function is used to calculate the number of cycles to wait in a SWD/JTAG clock
        // cycle, so we don't actually have to return the real CPU frequency.
        // cortex_m __delay divides by 2 and Cortex-M0+ needs 4 CPU cycles per delay loop iteration.
        125_000_000 / 2
    }
}

struct IoPin<'a> {
    pin: Flex<'a>,
    direction_pin: Option<Output<'a>>,
}

impl<'a> IoPin<'a> {
    fn new(pin: Peri<'a, impl Pin>, direction_pin: Option<Peri<'a, impl Pin>>) -> Self {
        Self {
            pin: Flex::new(pin),
            direction_pin: direction_pin.map(|pin| Output::new(pin, Level::Low)),
        }
    }
}

impl InputOutputPin for IoPin<'_> {
    fn set_as_output(&mut self) {
        self.pin.set_as_output();
        if let Some(dir) = self.direction_pin.as_mut() {
            dir.set_high();
        }
    }

    fn set_high(&mut self, high: bool) {
        match high {
            true => self.pin.set_high(),
            false => self.pin.set_low(),
        }
    }

    fn set_as_input(&mut self) {
        if let Some(dir) = self.direction_pin.as_mut() {
            dir.set_low();
        }
        self.pin.set_as_input();
    }

    fn is_high(&mut self) -> bool {
        self.pin.is_high()
    }
}

struct Leds<'a> {
    _red: Output<'a>,
    green: Output<'a>,
    blue: Output<'a>,
}

impl DapLeds for Leds<'_> {
    fn react_to_host_status(&mut self, host_status: dap::HostStatus) {
        match host_status {
            dap::HostStatus::Connected(c) => self.green.set_level(Level::from(c)),
            dap::HostStatus::Running(r) => self.blue.set_level(Level::from(r)),
        }
    }
}

struct NoSwo;

impl Swo for NoSwo {
    fn set_transport(&mut self, _transport: dap_rs::swo::SwoTransport) {
        todo!()
    }

    fn set_mode(&mut self, _mode: dap_rs::swo::SwoMode) {
        todo!()
    }

    fn set_baudrate(&mut self, _baudrate: u32) -> u32 {
        todo!()
    }

    fn set_control(&mut self, _control: dap_rs::swo::SwoControl) {
        todo!()
    }

    fn polling_data(&mut self, _buf: &mut [u8]) -> u32 {
        todo!()
    }

    fn streaming_data(&mut self) {
        todo!()
    }

    fn is_active(&self) -> bool {
        todo!()
    }

    fn bytes_available(&self) -> u32 {
        todo!()
    }

    fn buffer_size(&self) -> u32 {
        todo!()
    }

    fn support(&self) -> dap_rs::swo::SwoSupport {
        todo!()
    }

    fn status(&mut self) -> dap_rs::swo::SwoStatus {
        todo!()
    }
}

pub struct TargetPhysicallyConnected {
    pin: Input<'static>,
}

impl TargetPhysicallyConnected {
    /// This checks for the target being connected via the GND detect pin.
    pub fn target_detected(&mut self) -> bool {
        self.pin.is_low()
    }
}

pub struct TargetVccReader {
    pub pin: adc::Channel<'static>,
    pub adc: Adc<'static, adc::Blocking>,
}

impl TargetVccReader {
    pub fn read_voltage_mv(&mut self) -> u32 {
        let Self { pin, adc } = self;
        let val: u16 = adc.blocking_read(pin).unwrap();

        (2 * 3300 * val as u32) / 4095
    }
}

pub struct TranslatorPower {
    vtranslator_pwm: Pwm<'static>,
}

impl TranslatorPower {
    pub fn new(
        vtranslator_pin: Peri<'static, PIN_5>,
        vtranslator_pwm: Peri<'static, PWM_SLICE2>,
    ) -> Self {
        // Output channel B on PWM2 to GPIO 5
        let mut vtranslator_pwm = Pwm::new_output_b(vtranslator_pwm, vtranslator_pin, {
            let mut c = pwm::Config::default();
            c.top = 4095;
            c.enable = true;
            c.phase_correct = false;
            c
        });

        vtranslator_pwm.set_duty_cycle(1023).unwrap();

        Self { vtranslator_pwm }
    }

    pub fn set_translator_vcc(&mut self, mv: u32) {
        // ans = 0.0900 * 4095
        let v33 = 369; // duty cycle that give 3.3v

        // ans = 0.7617 * 4095
        let v18 = 3119; // duty cycle that gives 1.8v

        let vomin = 1800; // The actual voltage when PWM is set to v18 duty cycle
        let vomax = 3300; // The actual voltage when PWM is set to v33 duty cycle

        let mv = mv.min(vomax).max(vomin);

        let limit_diff = v18 - v33;
        let mv_diff = mv - vomin;

        let cnt = ((vomax - vomin - mv_diff) * limit_diff) / (vomax - vomin) + v33;

        self.vtranslator_pwm.set_duty_cycle(cnt as u16).unwrap();
    }
}

pub struct TargetPower {
    enable_5v_key: Output<'static>,
    enable_vtgt: Output<'static>,
    vtgt_pwm: Pwm<'static>,
}

impl TargetPower {
    pub fn enable_vtgt(&mut self) {
        self.enable_vtgt.set_high();
    }

    pub fn enable_5v_key(&mut self) {
        self.enable_5v_key.set_high();
    }

    pub fn new(
        enable_5v_key: Output<'static>,
        _enable_5v: Output<'static>,
        enable_vtgt: Output<'static>,
        vtgt_pin: Peri<'static, PIN_0>,
        vtgt_pwm: Peri<'static, PWM_SLICE0>,
    ) -> Self {
        // Output channel A on PWM0 to GPIO 0
        let mut vtgt_pwm = Pwm::new_output_a(vtgt_pwm, vtgt_pin, {
            let mut c = pwm::Config::default();
            c.top = 4095;
            c.enable = true;
            c.phase_correct = false;
            c
        });

        vtgt_pwm.set_duty_cycle(1023).unwrap();

        Self {
            enable_5v_key,
            enable_vtgt,
            vtgt_pwm,
        }
    }

    pub fn set_vtgt(&mut self, mv: u32) {
        let v33 = 369; // duty cycle that gives 3.3v
        let v18 = 3119; // duty cycle that gives 1.8v

        let vomin = 1800; // The actual voltage when PWM is set to v18 duty cycle
        let vomax = 3300; // The actual voltage when PWM is set to v33 duty cycle

        let mv = mv.min(vomax).max(vomin);

        let limit_diff = v18 - v33;
        let mv_diff = mv - vomin;

        let cnt = ((vomax - vomin - mv_diff) * limit_diff) / (vomax - vomin) + v33;

        self.vtgt_pwm.set_duty_cycle(cnt as u16).unwrap();
    }
}
