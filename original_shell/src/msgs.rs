// workerスレッドが受信するメッセージ
pub enum WorkerMsg {
    Signal(i32),
    Cmd(String),
}

// mainスレッドが受信するメッセージ
pub enum ShellMsg {
    Continue(i32), // 読み込みを再開。i32は終了コード
    Quit(i32),     // シェルを終了。i32はシェルの終了コード
}
