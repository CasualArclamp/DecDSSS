// made by CasualArclamp
//! The lines a rig is reached on: a serial port (CAT, and the DTR and RTS
//! lines that key some interfaces) or a TCP connection to Hamlib's `rigctld`.
//!
//! Adapted from voicemodem's `rig` crate (same author); the TCP link, the
//! control lines and the port descriptions are new.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serialport::{ClearBuffer, DataBits, FlowControl, Parity, SerialPortType, StopBits};

/// How long one read waits for bytes before saying there were none.
const POLL: Duration = Duration::from_millis(20);

/// How long a TCP connection is given to open.
const CONNECT: Duration = Duration::from_secs(2);

/// `rigctld`'s own port, when an address names none.
pub const RIGCTLD_PORT: u16 = 4532;

/// A serial control line that can key a transmitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line {
    Dtr,
    Rts,
}

impl Line {
    pub fn name(self) -> &'static str {
        match self {
            Line::Dtr => "DTR",
            Line::Rts => "RTS",
        }
    }
}

/// A byte link to a rig: its serial port, a `rigctld`, or a stand-in for tests.
pub trait Link: Send {
    /// Send all of `bytes`.
    fn send(&mut self, bytes: &[u8]) -> Result<(), String>;

    /// Append whatever has arrived to `into`, waiting a moment for something
    /// if nothing has. Nothing appended is not an error: the rig has said
    /// nothing yet.
    fn receive(&mut self, into: &mut Vec<u8>) -> Result<(), String>;

    /// Throw away whatever has arrived and not been read.
    fn discard(&mut self);

    fn set_baud(&mut self, baud: u32) -> Result<(), String>;

    /// Raise or lower a control line. Only a serial port has them.
    fn set_line(&mut self, _line: Line, _high: bool) -> Result<(), String> {
        Err("this link has no control lines".into())
    }
}

/// A serial port.
pub struct Serial {
    port: Box<dyn serialport::SerialPort>,
    name: String,
}

impl std::fmt::Debug for Serial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Serial")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Serial {
    /// Open the port `name` at `baud`: eight data bits, no parity, one stop
    /// bit, no flow control, and DTR and RTS held low from the start. Either
    /// line keys a rig whose menu says to key on it, so nothing here may
    /// raise one before it is asked to.
    pub fn open(name: &str, baud: u32) -> Result<Self, String> {
        let fail = |e: serialport::Error| format!("{name}: {e}");
        let mut port = serialport::new(name, baud)
            .data_bits(DataBits::Eight)
            .parity(Parity::None)
            .stop_bits(StopBits::One)
            .flow_control(FlowControl::None)
            .timeout(POLL)
            .dtr_on_open(false)
            .open()
            .map_err(|e| {
                format!("{name} would not open: {e} Is the rig on and plugged in, and no other program using the port?")
            })?;
        port.write_request_to_send(false).map_err(fail)?;
        port.write_data_terminal_ready(false).map_err(fail)?;
        Ok(Self {
            port,
            name: name.to_string(),
        })
    }
}

impl Link for Serial {
    fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.port
            .write_all(bytes)
            .and_then(|()| self.port.flush())
            .map_err(|e| format!("{}: {e}", self.name))
    }

    fn receive(&mut self, into: &mut Vec<u8>) -> Result<(), String> {
        let mut buffer = [0u8; 256];
        match self.port.read(&mut buffer) {
            Ok(n) => {
                into.extend_from_slice(&buffer[..n]);
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Ok(()),
            Err(e) => Err(format!("{}: {e}", self.name)),
        }
    }

    fn discard(&mut self) {
        let _ = self.port.clear(ClearBuffer::Input);
    }

    fn set_baud(&mut self, baud: u32) -> Result<(), String> {
        self.port
            .set_baud_rate(baud)
            .map_err(|e| format!("{}: {e}", self.name))
    }

    fn set_line(&mut self, line: Line, high: bool) -> Result<(), String> {
        match line {
            Line::Dtr => self.port.write_data_terminal_ready(high),
            Line::Rts => self.port.write_request_to_send(high),
        }
        .map_err(|e| format!("{}: {e}", self.name))
    }
}

/// A TCP connection, to Hamlib's `rigctld`.
pub struct Tcp {
    stream: TcpStream,
    name: String,
}

impl std::fmt::Debug for Tcp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tcp")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// `address` with `rigctld`'s port added if it has none: "localhost" is
/// "localhost:4532".
pub fn with_default_port(address: &str) -> String {
    let address = address.trim();
    if address.contains(':') {
        address.to_string()
    } else {
        format!("{address}:{RIGCTLD_PORT}")
    }
}

impl Tcp {
    /// Connect to `address`, "host:port" or just "host".
    pub fn open(address: &str) -> Result<Self, String> {
        let name = with_default_port(address);
        let fail = |e: std::io::Error| format!("{name}: {e}");
        let mut last = format!("{name}: no such address");
        for candidate in name.to_socket_addrs().map_err(fail)? {
            match TcpStream::connect_timeout(&candidate, CONNECT) {
                Ok(stream) => {
                    stream.set_read_timeout(Some(POLL)).map_err(fail)?;
                    stream.set_nodelay(true).map_err(fail)?;
                    return Ok(Self { stream, name });
                }
                Err(e) => last = format!("{name} would not answer: {e} Is rigctld running?"),
            }
        }
        Err(last)
    }
}

impl Link for Tcp {
    fn send(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.stream
            .write_all(bytes)
            .and_then(|()| self.stream.flush())
            .map_err(|e| format!("{}: {e}", self.name))
    }

    fn receive(&mut self, into: &mut Vec<u8>) -> Result<(), String> {
        let mut buffer = [0u8; 256];
        match self.stream.read(&mut buffer) {
            Ok(0) => Err(format!("{} closed the connection", self.name)),
            Ok(n) => {
                into.extend_from_slice(&buffer[..n]);
                Ok(())
            }
            // A read that timed out is how an empty line looks on a socket
            // (`WouldBlock` on Unix, `TimedOut` on Windows).
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                Ok(())
            }
            Err(e) => Err(format!("{}: {e}", self.name)),
        }
    }

    fn discard(&mut self) {
        // Read and drop whatever is waiting, without blocking.
        let _ = self.stream.set_nonblocking(true);
        let mut buffer = [0u8; 256];
        while matches!(self.stream.read(&mut buffer), Ok(n) if n > 0) {}
        let _ = self.stream.set_nonblocking(false);
    }

    fn set_baud(&mut self, _baud: u32) -> Result<(), String> {
        Ok(())
    }
}

/// A serial port on this machine: its name, and what it says it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Port {
    pub name: String,
    pub description: String,
}

/// The serial ports on this machine, in number order. A rig on USB shows up
/// as its bridge chip: the IC-7300 as a Silicon Labs CP210x, the FT-710 as a
/// CP2105 with two ports, of which CAT is on the Enhanced one.
pub fn ports() -> Vec<Port> {
    let Ok(found) = serialport::available_ports() else {
        return Vec::new();
    };
    let mut ports: Vec<Port> = found
        .into_iter()
        .map(|p| {
            let description = match p.port_type {
                SerialPortType::UsbPort(usb) => usb
                    .product
                    .or(usb.manufacturer)
                    .unwrap_or_else(|| format!("USB {:04X}:{:04X}", usb.vid, usb.pid)),
                SerialPortType::BluetoothPort => "Bluetooth".into(),
                SerialPortType::PciPort => "PCI".into(),
                SerialPortType::Unknown => String::new(),
            };
            Port {
                name: p.port_name,
                description,
            }
        })
        .collect();
    ports.sort_by_key(|p| (number(&p.name), p.name.clone()));
    ports
}

/// COM10 after COM9, not after COM1.
fn number(name: &str) -> u32 {
    name.trim_start_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_sort_by_number() {
        let mut names = vec!["COM10", "COM3", "COM1", "COM22"];
        names.sort_by_key(|n| number(n));
        assert_eq!(names, ["COM1", "COM3", "COM10", "COM22"]);
    }

    #[test]
    fn a_host_alone_gets_rigctlds_port() {
        assert_eq!(with_default_port("localhost"), "localhost:4532");
        assert_eq!(
            with_default_port(" 192.168.1.20:4533 "),
            "192.168.1.20:4533"
        );
    }
}
