//! Tells a machine's desktop which of the host's appearances to show, through
//! the guest program that paints it.

use crate::{
    node::{self, Target},
    protocol::{self, Request, Session, kind},
};
use gpui::{AsyncApp, AppContext};
use std::{
    cell::Cell,
    rc::Rc,
    time::Duration,
};

/// The guest program that writes the desktop's configs and repaints it.
const APPEARANCE: &str = "/usr/local/libexec/varmint-appearance";
/// How often to try while a machine is still booting.
const RETRY: Duration = Duration::from_millis(500);
/// How many tries a machine's desktop gets to come up.
const ATTEMPTS: usize = 60;

/// Sends the current appearance to the desktop of machine `name`, trying
/// again while its guest boots. `dark` changes as the host does, so a send
/// that is under way when it changes follows the new appearance.
pub async fn show(name: String, dark: Rc<Cell<bool>>, cx: &mut AsyncApp) {
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            cx.background_executor().timer(RETRY).await;
        }
        let mode = dark.get();
        let machine = name.clone();
        if cx.background_spawn(async move { send(&machine, mode) }).await.is_ok() && dark.get() == mode {
            return;
        }
    }
}

/// Runs the appearance program in the guest and waits for it to finish.
fn send(name: &str, dark: bool) -> Result<(), String> {
    let target = Target::resolve(name)?;
    let mode = match dark {
        true => "dark",
        false => "light",
    };
    let session = Session::Exec { argv: vec![APPEARANCE.to_owned(), mode.to_owned()] };
    let mut control = node::request(&target, &Request::Open { session })?;
    loop {
        let Some(frame) = protocol::read_frame(&mut control.reader)
            .map_err(|error| format!("VM {}: {error}", target.qualified_name()))?
        else {
            return Err(format!("VM {} closed the connection", target.qualified_name()));
        };
        match frame.kind {
            kind::EXIT => {
                let payload = frame.payload;
                let code = i32::from_le_bytes(
                    payload
                        .try_into()
                        .map_err(|_| format!("VM {} sent a malformed exit frame", target.qualified_name()))?,
                );
                return match code {
                    0 => Ok(()),
                    code => Err(format!(
                        "the desktop of VM {} failed to change appearance: exit code {code}",
                        target.qualified_name()
                    )),
                };
            }
            kind::DATA | kind::STDERR => {}
            other => return Err(format!("unexpected frame kind {other} from VM {}", target.qualified_name())),
        }
    }
}
