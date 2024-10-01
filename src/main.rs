#![no_std]
#![no_main]

use embassy_sync::signal::Signal;
use esp_backtrace as _;

use esp_hal::i2c::I2C;
use esp_hal::peripherals::I2C0;
use esp_hal::Async;
use esp_println::{self as _};

use esp_println::println;

use embassy_executor::Spawner;
use embassy_time::{Duration, Ticker, Timer};

use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex, RawMutex};

use esp_hal::{
    clock::ClockControl,
    gpio::{Input, Io, Pull},
    peripherals::Peripherals,
    prelude::*,
    rmt::Rmt,
    rng::Rng,
    system::SystemControl,
    timer::{timg::TimerGroup, ErasedTimer, OneShotTimer},
};

use esp_wifi::{
    initialize,
    wifi::{
        ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent, WifiStaDevice,
        WifiState,
    },
    EspWifiInitFor,
};

use embassy_net::{tcp::TcpSocket, Stack, StackResources};

use esp_hal_smartled::{smartLedBuffer, SmartLedsAdapter};
use log::error;
use smart_leds::{SmartLedsWrite, RGB8};
use static_cell::StaticCell;

static STATIC_CELL: StaticCell<[OneShotTimer<ErasedTimer>; 1]> = StaticCell::new();

const LEDNUM: usize = 1;

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");

const TCP_LISTEN_PORT: u16 = 9000;

use sht3x::SHT3x;

struct TempData {
    t: sht3x::Tmp,
    h: sht3x::Hum,
}

type TempSignal = Signal<CriticalSectionRawMutex, TempData>;

static TEMPDATA: TempSignal = TempSignal::new();

#[embassy_executor::task]
async fn tempdata_handle(mut i2c1: I2C<'static, I2C0, Async>) {
    let mut sht = SHT3x::new(&mut i2c1);

    let _ = sht.reset().await;
    let mut tick = Ticker::every(Duration::from_hz(1));
    Timer::after(Duration::from_millis(10)).await;

    while sht.write(sht3x::CMD::AUTO_1MPS_HIGH).await.is_err() {
        tick.next().await;
    }

    loop {
        tick.next().await;
        match sht.get_measurement().await {
            Ok((t, h)) => {
                let temp: i16 = t.into();
                let hum: u8 = h.into();
                println!("t: {:?} h {:?}", temp, hum);
                TEMPDATA.signal(TempData { t, h });
            }
            Err(e) => {
                println!("TEMP: I2C error: {:?}", e);
            }
        }
    }
}

type NetStack = Stack<WifiDevice<'static, WifiStaDevice>>;

static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
static STACK: StaticCell<NetStack> = StaticCell::new();

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    println!("Init!");
    let peripherals = Peripherals::take();
    let system = SystemControl::new(peripherals.SYSTEM);
    let clocks = ClockControl::boot_defaults(system.clock_control).freeze();

    let timg0 = TimerGroup::new(peripherals.TIMG0, &clocks);
    let timg1 = TimerGroup::new(peripherals.TIMG1, &clocks);

    let timer0: ErasedTimer = timg0.timer0.into();
    let timers = [OneShotTimer::new(timer0)];

    let timers = STATIC_CELL.init(timers);

    let io = Io::new(peripherals.GPIO, peripherals.IO_MUX);

    let init = initialize(
        EspWifiInitFor::Wifi,
        timg1.timer0,
        Rng::new(peripherals.RNG),
        peripherals.RADIO_CLK,
        &clocks,
    )
    .unwrap();

    let i2c0 = I2C::new_async(
        peripherals.I2C0,
        io.pins.gpio11,
        io.pins.gpio10,
        400.kHz(),
        &clocks,
    );

    let wifi = peripherals.WIFI;
    let (wifi_interface, controller) =
        esp_wifi::wifi::new_with_mode(&init, wifi, WifiStaDevice).unwrap();

    esp_hal_embassy::init(&clocks, timers);

    let config = embassy_net::Config::dhcpv4(Default::default());
    let seed = 1234; // very random, very secure seed

    // Init network stack
    let stack = STACK.init(Stack::new(
        wifi_interface,
        config,
        RESOURCES.init(StackResources::<3>::new()),
        seed,
    ));

    // spawner.must_spawn(tempdata_handle(i2c0));

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(stack)).ok();

    let knopje = Input::new(io.pins.gpio9, Pull::Up);

    let rmt = Rmt::new(peripherals.RMT, 80u32.MHz(), &clocks).unwrap();
    let rmt_buffer = smartLedBuffer!(1);
    let mut led = SmartLedsAdapter::new(rmt.channel0, io.pins.gpio8, rmt_buffer, &clocks);

    let mut data = [RGB8::default(); LEDNUM];

    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    println!("Waiting to get IP address...");
    let local_addr = loop {
        if let Some(config) = stack.config_v4() {
            println!("Got IP: {}", config.address);
            break config.address.address();
        }
        Timer::after(Duration::from_millis(500)).await;
    };

    println!("Init2!");

    for idx in 0..4 {
        println!("Spawn TcpThreads: {}", idx);
        if let Err(e) = spawner.spawn(handle_tcp_connection(stack, idx)) {
            error!("Failed to spawn thread id {}, {:?}", idx, e);
        }
    }

    loop {
        Timer::after_secs(10).await;
        println!("Main thread: Still alive");
    }
    //     let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    //     socket.set_timeout(Some(Duration::from_secs(2)));

    //     println!("Listening on tcp://{}:{}...", local_addr, TCP_LISTEN_PORT);
    //     if let Err(e) = socket.accept(TCP_LISTEN_PORT).await {
    //         error!("accept error: {:?}", e);
    //         continue;
    //     }

    //     if let Some(remote_ip) = socket.remote_endpoint() {
    //         println!("Connection from: {}", remote_ip);
    //     } else {
    //         println!("Connection...");
    //     }
    //     loop {
    //         let n = match socket.read(&mut sock_buf).await {
    //             Ok(0) => {
    //                 println!("read EOF");
    //                 break;
    //             }
    //             Ok(n) => n,
    //             Err(e) => {
    //                 println!("Error: {:?}", e);
    //                 break;
    //             }
    //         };

    //         println!("Got: {:02x?}", &sock_buf[0..n]);

    //         let col = match sock_buf[0] {
    //             b'r' => RGB8::new(0x00, 0x00, 0x80),
    //             b'g' => RGB8::new(0x00, 0x80, 0x00),
    //             b'b' => RGB8::new(0x80, 0x00, 0x00),
    //             _ => RGB8::new(0x1, 0x1, 0x1),
    //         };
    //         data[0] = col;
    //         led.write(data.iter().cloned()).unwrap();
    //     }
    // }
}

#[embassy_executor::task]
async fn handle_tcp_connection(stack: &'static NetStack, idx: usize) -> ! {
    let mut rx_buffer = [0; 1500];
    let mut tx_buffer = [0; 300];

    let mut sock_buf = [0; 1500];

    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(10)));

        println!("[{}]: Listening on tcp://:{}...", idx, TCP_LISTEN_PORT);
        if let Err(e) = socket.accept(TCP_LISTEN_PORT).await {
            error!("[{}]: accept error: {:?}", idx, e);
            continue;
        }

        if let Some(remote_ip) = socket.remote_endpoint() {
            println!("[{}]: Connection from: {}", idx, remote_ip);
        } else {
            println!("[{}]: Connection...", idx);
        }
        loop {
            let n = match socket.read(&mut sock_buf).await {
                Ok(0) => {
                    println!("[{}]: read EOF", idx);
                    break;
                }
                Ok(n) => n,
                Err(e) => {
                    println!("[{}]: Error: {:?}", idx, e);
                    break;
                }
            };
            println!("[{}]: got {:02?}", idx, &sock_buf[0..n]);
        }
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    println!("Device capabilities: {:?}", controller.get_capabilities());
    loop {
        if esp_wifi::wifi::get_wifi_state() == WifiState::StaConnected {
            // wait until we're no longer connected
            controller.wait_for_event(WifiEvent::StaDisconnected).await;
            Timer::after(Duration::from_millis(5000)).await
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::Client(ClientConfiguration {
                ssid: SSID.try_into().unwrap(),
                password: PASSWORD.try_into().unwrap(),
                ..Default::default()
            });
            controller.set_configuration(&client_config).unwrap();
            println!("Starting wifi");
            controller.start().await.unwrap();
            println!("Wifi started!");
        }
        println!("About to connect...");

        match controller.connect().await {
            Ok(_) => println!("Wifi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(stack: &'static NetStack) {
    stack.run().await
}
