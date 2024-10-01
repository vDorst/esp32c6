#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_println::{self as _};

use esp_println::println;

use embassy_executor::Spawner;
use embassy_net::{tcp::TcpSocket, Stack, StackResources};
use embassy_time::{Duration, Timer};
use esp_hal::{
    clock::ClockControl,
    gpio::Io,
    peripherals::Peripherals,
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
use log::error;
use static_cell::StaticCell;

static STATIC_CELL: StaticCell<[OneShotTimer<ErasedTimer>; 1]> = StaticCell::new();

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");

const TCP_LISTEN_PORT: u16 = 9000;

const RX_BUFFER: usize = 1500;
const TX_BUFFER: usize = 1500;
const RECV_BUFFER: usize = 300;
const TASK_BUFFER: usize = RX_BUFFER + TX_BUFFER + RECV_BUFFER;

/// Number TCP connections to handle
const TASKS: usize = 8;

type NetStack = Stack<WifiDevice<'static, WifiStaDevice>>;

/// Network Resources.
/// Need to specify the number of sockets we want to use.
/// N_TASKS + 1 DHCP Client.
static RESOURCES: StaticCell<StackResources<{ 1 + TASKS }>> = StaticCell::new();
static STACK: StaticCell<NetStack> = StaticCell::new();
static NETWORKBUFFER: StaticCell<[u8; TASK_BUFFER * TASKS]> = StaticCell::new();

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

    let _io = Io::new(peripherals.GPIO, peripherals.IO_MUX);

    let init = initialize(
        EspWifiInitFor::Wifi,
        timg1.timer0,
        Rng::new(peripherals.RNG),
        peripherals.RADIO_CLK,
        &clocks,
    )
    .unwrap();

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
        RESOURCES.init(StackResources::new()),
        seed,
    ));

    let net_buffer = NETWORKBUFFER.init([0; TASKS * TASK_BUFFER]);

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(stack)).ok();

    println!("Waiting for link up...");
    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(100)).await;
    }

    println!("Waiting to get IP address...");
    let _local_addr = loop {
        if let Some(config) = stack.config_v4() {
            println!("Got IP: {}", config.address);
            break config.address.address();
        }
        Timer::after(Duration::from_millis(100)).await;
    };

    for (idx, buf) in net_buffer.chunks_exact_mut(TASK_BUFFER).enumerate() {
        println!("Spawn TcpThreads: {}", idx);
        if let Err(e) = spawner.spawn(handle_tcp_connection(stack, buf, idx)) {
            error!("Failed to spawn thread id {}, {:?}", idx, e);
        }
    }

    loop {
        Timer::after_secs(10).await;
        println!("Main thread: Still alive");
    }
}

#[embassy_executor::task(pool_size = TASKS)]
async fn handle_tcp_connection(
    stack: &'static NetStack,
    buffer: &'static mut [u8],
    idx: usize,
) -> ! {
    let (sock_buf, buffer) = buffer.split_at_mut(RECV_BUFFER);
    let (rx_buffer, tx_buffer) = buffer.split_at_mut(RX_BUFFER);

    loop {
        let mut socket = TcpSocket::new(stack, rx_buffer, tx_buffer);
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
            let n = match socket.read(sock_buf).await {
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
