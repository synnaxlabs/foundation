use std::num::NonZeroU8;
use std::sync::Mutex;

use env::clock::Clock;
use env::serial::{self, Serial};

use super::line::Line;
use crate::device::Device;

/// Opens the port at `config.path` and answers each request frame for `unit` on it
/// from `device`, until the port fails, and gives that error. A broadcast (unit 0)
/// changes the device and gets no reply. A frame that is not valid gets no reply,
/// and the bytes after it are dropped until the line is quiet for 3.5 characters,
/// as a real device does. Tests and simulations use it as the far side of a line.
///
/// # Panics
///
/// When a thread panicked while it held the lock on `device`.
pub async fn serve(
    serial: &Serial,
    config: &serial::Config,
    clock: Clock,
    unit: NonZeroU8,
    device: &Mutex<Device>,
) -> serial::Error {
    let port = match serial.open(config).await {
        Ok(port) => port,
        Err(error) => return error,
    };
    let mut line = Line::new(port, clock, config.settings);
    let (mut bytes, mut reply) = (Vec::new(), Vec::new());
    let mut bad = false;
    loop {
        let deadline = (!bytes.is_empty()).then(|| line.rested());
        match line.read(&mut bytes, deadline).await {
            Ok(true) => {}
            Ok(false) => {
                bytes.clear();
                bad = false;
            }
            Err(error) => return error,
        }
        while !bad {
            let frame = match super::decode_request(&bytes) {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(_) => {
                    bad = true;
                    break;
                }
            };
            let (len, to) = (frame.len, frame.unit);
            if to == 0 || to == unit.get() {
                reply.push(to);
                device
                    .lock()
                    .expect("no panic under the device lock")
                    .answer(frame.pdu, &mut reply);
            }
            bytes.drain(..len);
            if to == unit.get() {
                super::seal(&mut reply, 0);
                let sent = match line.rest(None, None).await {
                    Ok(_) => line.write(&reply, None).await,
                    Err(error) => Err(error),
                };
                if let Err(error) = sent {
                    return error;
                }
            }
            reply.clear();
        }
    }
}
