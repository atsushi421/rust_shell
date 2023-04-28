use crate::helper::DynError;
use crate::msgs::WorkerMsg;
use signal_hook::{consts::*, iterator::Signals};
use std::{sync::mpsc::Sender, thread};

pub fn spawn_sig_handler(tx: Sender<WorkerMsg>) -> Result<(), DynError> {
    let mut signals = Signals::new(&[SIGCHLD, SIGINT, SIGTSTP])?;
    thread::spawn(move || {
        for sig in signals.forever() {
            // シグナルを受信し、workerスレッドに送信
            tx.send(WorkerMsg::Signal(sig)).unwrap();
        }
    });

    Ok(())
}
