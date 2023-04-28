use crate::helper::DynError;
use crate::msgs::{ShellMsg, WorkerMsg};
use crate::signal_handler::spawn_sig_handler;
use crate::worker::Worker;
use nix::sys::signal::{signal, SigHandler, Signal};
use rustyline::{error::ReadlineError, Editor};
use std::{
    process::exit,
    sync::mpsc::{channel, sync_channel},
};

#[derive(Debug)]
pub struct Shell {
    logfile: String,
}

impl Shell {
    pub fn new(logfile: &str) -> Self {
        Shell {
            logfile: logfile.to_string(),
        }
    }

    // main スレッド
    pub fn run(&self) -> Result<(), DynError> {
        unsafe { signal(Signal::SIGTTOU, SigHandler::SigIgn).unwrap() };

        let mut rl = Editor::<()>::new()?;
        if let Err(e) = rl.load_history(&self.logfile) {
            eprintln!("Atsush: ヒストリファイルの読み込みに失敗: {e}");
        }

        // チャネルを作成し、workerスレッドとsignal_handlerスレッドを生成
        let (worker_tx, worker_rx) = channel();
        let (shell_tx, shell_rx) = sync_channel(0);
        spawn_sig_handler(worker_tx.clone())?;
        Worker::new().spawn(worker_rx, shell_tx);

        let exit_val;
        let mut prev = 0; // 直前の終了コード
        loop {
            // 1行読み込んで、その行をworkerスレッドに送信
            let face = if prev == 0 { '\u{1F642}' } else { '\u{1F480}' }; // 絵文字
            match rl.readline(&format!("Atsush {face} %> ")) {
                Ok(line) => {
                    let line_trimed = line.trim();
                    if line_trimed.is_empty() {
                        continue;
                    } else {
                        rl.add_history_entry(line_trimed);
                    }

                    // workerスレッドに送信
                    worker_tx.send(WorkerMsg::Cmd(line)).unwrap();
                    // workerスレッドの処理が完了するまで待機し、終了コードを受信
                    match shell_rx.recv().unwrap() {
                        ShellMsg::Continue(n) => prev = n, // 読み込み再開
                        ShellMsg::Quit(n) => {
                            // シェル終了
                            exit_val = n;
                            break;
                        }
                    }
                }
                // コマンド読み込み時に割り込みが発生した場合、再読込
                Err(ReadlineError::Interrupted) => {
                    eprintln!("Atsush: 終了はCtrl+d");
                }
                // Ctrl+dが押されたら終了
                Err(ReadlineError::Eof) => {
                    worker_tx.send(WorkerMsg::Cmd("exit".to_string())).unwrap();
                    match shell_rx.recv().unwrap() {
                        ShellMsg::Quit(n) => {
                            // シェル終了
                            exit_val = n;
                            break;
                        }
                        _ => panic!("exitに失敗"),
                    }
                }
                // 何らかの理由で読み込みに失敗したら終了
                Err(e) => {
                    eprintln!("Atsush: 読み込みエラー\n{e}");
                    exit_val = 1;
                    break;
                }
            }
        }

        if let Err(e) = rl.save_history(&self.logfile) {
            eprintln!("Atsush: ヒストリファイルの書き込みに失敗: {e}");
        }
        exit(exit_val);
    }
}
