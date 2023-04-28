use crate::helper::DynError;
use crate::msgs::{ShellMsg, WorkerMsg};
use nix::{
    libc,
    sys::{
        signal::{killpg, Signal},
        wait::{waitpid, WaitPidFlag, WaitStatus},
    },
    unistd::{self, dup2, execvp, fork, pipe, setpgid, tcgetpgrp, tcsetpgrp, ForkResult, Pid},
};

use signal_hook::consts::*;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ffi::CString,
    mem::replace,
    sync::mpsc::{Receiver, SyncSender},
    thread,
};

/// ドロップ時にクロージャfを呼び出す型
struct CleanUp<F>
where
    F: Fn(),
{
    f: F,
}

impl<F> Drop for CleanUp<F>
where
    F: Fn(),
{
    fn drop(&mut self) {
        (self.f)()
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
enum ProcState {
    Run,  // 実行中
    Stop, // 停止中
}

#[derive(Debug, Clone)]
struct ProcInfo {
    state: ProcState, // 実行状態
    pgid: Pid,        // プロセスグループID
}

#[derive(Debug)]
pub struct Worker {
    exit_val: i32,   // 終了コード
    fg: Option<Pid>, // フォアグラウンドプロセスグループID

    // ジョブIDから、プロセスグループID・実行コマンドへのマップ
    jobs: BTreeMap<usize, (Pid, String)>,

    // プロセスグループIDから、ジョブID・プロセスIDへのマップ
    pgid_to_pids: HashMap<Pid, (usize, HashSet<Pid>)>,

    pid_to_info: HashMap<Pid, ProcInfo>, // プロセスIDから、プロセスグループIDへのマップ
    shell_pgid: Pid,                     // シェルのプロセスグループID
}

type CmdResult<'a> = Result<Vec<(&'a str, Vec<&'a str>)>, DynError>;

impl Worker {
    pub fn new() -> Self {
        Worker {
            exit_val: 0,
            fg: None, // フォアグラウンドはシェル
            jobs: BTreeMap::new(),
            pgid_to_pids: HashMap::new(),
            pid_to_info: HashMap::new(),

            // シェルのプロセスグループIDを取得
            shell_pgid: tcgetpgrp(libc::STDIN_FILENO).unwrap(),
        }
    }

    // workerスレッドを生成
    pub fn spawn(mut self, worker_rx: Receiver<WorkerMsg>, shell_tx: SyncSender<ShellMsg>) {
        thread::spawn(move || {
            for msg in worker_rx.iter() {
                // メッセージを受信
                match msg {
                    WorkerMsg::Cmd(line) => {
                        match parse_cmd(&line) {
                            Ok(cmd) => {
                                if self.built_in_cmd(&cmd, &shell_tx) {
                                    // 組み込みコマンドなら、worker_rxから受信
                                    continue;
                                }

                                if !self.spawn_child(&line, &cmd) {
                                    // 子プロセス生成に失敗した場合、シェルからの入力を再開
                                    shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap();
                                }
                            }
                            Err(e) => {
                                eprintln!("Atsush: {e}");
                                shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap();
                            }
                        }
                    }
                    WorkerMsg::Signal(SIGCHLD) => {
                        self.wait_child(&shell_tx); // 子プロセスの状態変化管理
                    }
                    _ => (),
                }
            }
        });
    }

    fn built_in_cmd(&mut self, cmd: &[(&str, Vec<&str>)], shell_tx: &SyncSender<ShellMsg>) -> bool {
        if cmd.len() > 1 {
            return false; // 組み込みコマンドのパイプは非対応
        }

        match cmd[0].0 {
            "exit" => self.run_exit(&cmd[0].1, shell_tx),
            "jobs" => self.run_jobs(shell_tx),
            "fg" => self.run_fg(&cmd[0].1, shell_tx),
            "cd" => self.run_cd(&cmd[0].1, shell_tx),
            _ => false,
        }
    }

    /// exitコマンドを実行
    ///
    /// 第1引数が指定された場合、それを終了コードとしてシェルを終了。
    /// 引数がない場合は、最後に終了したプロセスの終了コードとしてシェルを終了。
    fn run_exit(&mut self, args: &[&str], shell_tx: &SyncSender<ShellMsg>) -> bool {
        // 実行中のジョブがある場合は終了しない
        if !self.jobs.is_empty() {
            eprintln!("ジョブが実行中なので終了できません");
            self.exit_val = 1; // 失敗
            shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap(); // シェルを再開
            return true;
        }

        // 終了コードを取得
        let exit_val = if let Some(s) = args.get(1) {
            if let Ok(n) = (*s).parse::<i32>() {
                n
            } else {
                // 終了コードか整数ではない
                eprintln!("{s}は不正な引数です");
                self.exit_val = 1; // 失敗
                shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap(); // シェルを再開
                return true;
            }
        } else {
            self.exit_val
        };

        shell_tx.send(ShellMsg::Quit(exit_val)).unwrap(); // シェルを終了
        true
    }

    /// 現在シェルが管理して実行しているジョブ一覧を表示
    /// worker::jobsを表示
    /// 表示後、shell_txにShellMsg::Continueを送信してシェルを再開させ、trueを返す
    fn run_jobs(&mut self, shell_tx: &SyncSender<ShellMsg>) -> bool {
        todo!();
    }

    /// cdコマンドを実行
    fn run_cd(&mut self, args: &[&str], shell_tx: &SyncSender<ShellMsg>) -> bool {
        todo!();
    }

    /// fgコマンドを実行
    fn run_fg(&mut self, args: &[&str], shell_tx: &SyncSender<ShellMsg>) -> bool {
        self.exit_val = 1; // とりあえず失敗に設定

        // 引数をチェック
        if args.len() < 2 {
            eprintln!("usage: fg 数字");
            shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap(); // シェルを再開
            return true;
        }

        // ジョブIDを取得
        if let Ok(n) = args[1].parse::<usize>() {
            if let Some((pgid, cmd)) = self.jobs.get(&n) {
                eprintln!("[{n}] 再開\t{cmd}");

                // フォアグラウンドプロセスに設定
                self.fg = Some(*pgid);
                tcsetpgrp(libc::STDIN_FILENO, *pgid).unwrap();

                // ジョブの実行を再開
                killpg(*pgid, Signal::SIGCONT).unwrap();
                return true;
            }
        }

        // 失敗
        eprintln!("{}というジョブは見つかりませんでした", args[1]);
        shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap(); // シェルを再開
        true
    }

    /// 子プロセスを生成。失敗した場合はシェルからの入力を再開させる必要あり
    fn spawn_child(&mut self, line: &str, cmd: &[(&str, Vec<&str>)]) -> bool {
        assert_ne!(cmd.len(), 0); // コマンドが空でないか検査

        // ジョブIDを取得
        let job_id = if let Some(id) = self.get_new_job_id() {
            id
        } else {
            eprintln!("Atsush: 管理可能なジョブの最大値に到達");
            return false;
        };

        if cmd.len() > 2 {
            eprintln!("Atsush: 三つ以上のコマンドによるパイプはサポートしていません");
            return false;
        }

        let mut input = None; // 二つめのプロセスの標準入力
        let mut output = None; // 一つめのプロセスの標準出力
        if cmd.len() == 2 {
            // パイプを作成
            let p = pipe().unwrap();
            input = Some(p.0);
            output = Some(p.1);
        }

        // パイプを閉じる関数を定義
        let cleanup_pipe = CleanUp {
            f: || {
                if let Some(fd) = input {
                    syscall(|| unistd::close(fd)).unwrap();
                }
                if let Some(fd) = output {
                    syscall(|| unistd::close(fd)).unwrap();
                }
            },
        };

        let pgid;
        // 一つめのプロセスを生成
        match fork_exec(Pid::from_raw(0), cmd[0].0, &cmd[0].1, None, output) {
            Ok(child) => {
                pgid = child;
            }
            Err(e) => {
                eprintln!("Atsush: プロセス生成エラー: {e}");
                return false;
            }
        }

        // プロセス、ジョブの情報を追加
        let info = ProcInfo {
            state: ProcState::Run,
            pgid,
        };
        let mut pids = HashMap::new();
        pids.insert(pgid, info.clone()); // 一つめのプロセスの情報

        // 二つめのプロセスを生成
        if cmd.len() == 2 {
            match fork_exec(pgid, cmd[1].0, &cmd[1].1, input, None) {
                Ok(child) => {
                    pids.insert(child, info);
                }
                Err(e) => {
                    eprintln!("Atsush: プロセス生成エラー: {e}");
                    return false;
                }
            }
        }

        std::mem::drop(cleanup_pipe); // パイプをクローズ

        // ジョブ情報を追加し、子プロセスをフォアグラウンドに
        self.fg = Some(pgid);
        self.insert_job(job_id, pgid, pids, line);
        tcsetpgrp(libc::STDIN_FILENO, pgid).unwrap();

        true
    }

    /// 子プロセスの状態変化を管理
    fn wait_child(&mut self, shell_tx: &SyncSender<ShellMsg>) {
        // WUNTRACED: 子プロセスの停止
        // WNOHANG: ブロックしない
        // WCONTINUED: 実行再開時
        let flag = Some(WaitPidFlag::WUNTRACED | WaitPidFlag::WNOHANG | WaitPidFlag::WCONTINUED);

        loop {
            match syscall(|| waitpid(Pid::from_raw(-1), flag)) {
                Ok(WaitStatus::Exited(pid, status)) => {
                    // プロセスが終了
                    self.exit_val = status; // 終了コードを保存
                    self.process_term(pid, shell_tx);
                }
                Ok(WaitStatus::Signaled(pid, sig, core)) => {
                    // プロセスがシグナルにより終了
                    eprintln!(
                        "\nAtsush: 子プロセスがシグナルにより終了{}: pid = {pid}, signal = {sig}",
                        if core { "（コアダンプ）" } else { "" }
                    );
                    self.exit_val = sig as i32 + 128; // 終了コードを保存
                    self.process_term(pid, shell_tx);
                }
                // プロセスが停止
                Ok(WaitStatus::Stopped(pid, _sig)) => self.process_stop(pid, shell_tx),
                // プロセスが実行再開
                Ok(WaitStatus::Continued(pid)) => self.process_continue(pid),
                Ok(WaitStatus::StillAlive) => return, // waitすべき子プロセスはいない
                Err(nix::Error::ECHILD) => return,    // 子プロセスはいない
                Err(e) => {
                    eprintln!("\nAtsush: waitが失敗: {e}");
                    exit(1);
                }
                #[cfg(any(target_os = "linux", target_os = "android"))]
                Ok(WaitStatus::PtraceEvent(pid, _, _) | WaitStatus::PtraceSyscall(pid)) => {
                    self.process_stop(pid, shell_tx)
                }
            }
        }
    }

    /// プロセスの再開処理
    fn process_continue(&mut self, pid: Pid) {
        self.set_pid_state(pid, ProcState::Run);
    }

    /// プロセスの停止処理
    fn process_stop(&mut self, pid: Pid, shell_tx: &SyncSender<ShellMsg>) {
        self.set_pid_state(pid, ProcState::Stop); // プロセスを停止中に設定
        let pgid = self.pid_to_info.get(&pid).unwrap().pgid; // プロセスグループIDを取得
        let job_id = self.pgid_to_pids.get(&pgid).unwrap().0; // ジョブIDを取得
        self.manage_job(job_id, pgid, shell_tx); // 必要ならフォアグラウンドプロセスをシェルに設定
    }

    /// プロセスの終了処理
    fn process_term(&mut self, pid: Pid, shell_tx: &SyncSender<ShellMsg>) {
        // プロセスの情報を削除し、必要ならフォアグラウンドプロセスをシェルに設定
        if let Some((job_id, pgid)) = self.remove_pid(pid) {
            self.manage_job(job_id, pgid, shell_tx);
        }
    }

    /// ジョブの管理。引数には変化のあったジョブとプロセスグループを指定
    ///
    /// - フォアグラウンドプロセスが空の場合、シェルをフォアグラウンドに設定
    /// - フォアグラウンドプロセスがすべて停止中の場合、シェルをフォアグラウンドに設定
    fn manage_job(&mut self, job_id: usize, pgid: Pid, shell_tx: &SyncSender<ShellMsg>) {
        let is_fg = self.fg.map_or(false, |x| pgid == x); // フォアグラウンドのプロセスか？
        let line = &self.jobs.get(&job_id).unwrap().1;
        if is_fg {
            // 状態が変化したプロセスはフォアグラウンド
            if self.is_group_empty(pgid) {
                // フォアグラウンドプロセスが空の場合、
                // ジョブ情報を削除しシェルをフォアグラウンドに設定
                eprintln!("[{job_id}] 終了\t{line}");
                self.remove_job(job_id);
                self.set_shell_fg(shell_tx);
            } else if self.is_group_stop(pgid).unwrap() {
                // フォアグラウンドプロセスがすべて停止中の場合、シェルをフォアグラウンドに設定
                eprintln!("\n[{job_id}] 停止\t{line}");
                self.set_shell_fg(shell_tx);
            }
        } else {
            // プロセスグループが空の場合、ジョブ情報を削除
            if self.is_group_empty(pgid) {
                eprintln!("\n[{job_id}] 終了\t{line}");
                self.remove_job(job_id);
            }
        }
    }

    /// 空のプロセスグループなら真
    fn is_group_empty(&self, pgid: Pid) -> bool {
        self.pgid_to_pids.get(&pgid).unwrap().1.is_empty()
    }

    /// プロセスグループのプロセスすべてが停止中なら真
    fn is_group_stop(&self, pgid: Pid) -> Option<bool> {
        for pid in self.pgid_to_pids.get(&pgid)?.1.iter() {
            if self.pid_to_info.get(pid).unwrap().state == ProcState::Run {
                return Some(false);
            }
        }
        Some(true)
    }

    /// 新たなジョブIDを取得
    fn get_new_job_id(&self) -> Option<usize> {
        if let Some(i) = (0..=usize::MAX).find(|&i| !self.jobs.contains_key(&i)) {
            return Some(i);
        }
        None
    }

    /// シェルをフォアグラウンドに設定
    fn set_shell_fg(&mut self, shell_tx: &SyncSender<ShellMsg>) {
        self.fg = None;
        tcsetpgrp(libc::STDIN_FILENO, self.shell_pgid).unwrap();
        shell_tx.send(ShellMsg::Continue(self.exit_val)).unwrap();
    }

    /// 新たなジョブ情報を追加
    ///
    /// - job_id: ジョブID
    /// - pgid: プロセスグループID
    /// - pids: プロセス
    fn insert_job(&mut self, job_id: usize, pgid: Pid, pids: HashMap<Pid, ProcInfo>, line: &str) {
        assert!(!self.jobs.contains_key(&job_id));
        self.jobs.insert(job_id, (pgid, line.to_string())); // ジョブ情報を追加

        let mut procs = HashSet::new(); // pgid_to_pidsへ追加するプロセス
        for (pid, info) in pids {
            procs.insert(pid);

            assert!(!self.pid_to_info.contains_key(&pid));
            self.pid_to_info.insert(pid, info); // プロセスの情報を追加
        }

        assert!(!self.pgid_to_pids.contains_key(&pgid));
        self.pgid_to_pids.insert(pgid, (job_id, procs)); // プロセスグループの情報を追加
    }

    /// プロセスの実行状態を設定し、以前の状態を返す。
    /// pidが存在しないプロセスの場合はNoneを返す。
    fn set_pid_state(&mut self, pid: Pid, state: ProcState) -> Option<ProcState> {
        let info = self.pid_to_info.get_mut(&pid)?;
        Some(replace(&mut info.state, state))
    }

    /// プロセスの情報を削除し、削除できた場合プロセスの所属する。
    /// (ジョブID, プロセスグループID)を返す。
    /// 存在しないプロセスの場合はNoneを返す。
    fn remove_pid(&mut self, pid: Pid) -> Option<(usize, Pid)> {
        let pgid = self.pid_to_info.get(&pid)?.pgid; // プロセスグループIDを取得
        let it = self.pgid_to_pids.get_mut(&pgid)?;
        it.1.remove(&pid); // プロセスグループからpidを削除
        let job_id = it.0; // ジョブIDを取得
        Some((job_id, pgid))
    }

    /// ジョブ情報を削除し、関連するプロセスグループの情報も削除
    fn remove_job(&mut self, job_id: usize) {
        if let Some((pgid, _)) = self.jobs.remove(&job_id) {
            if let Some((_, pids)) = self.pgid_to_pids.remove(&pgid) {
                assert!(pids.is_empty()); // ジョブを削除するときはプロセスグループは空のはず
            }
        }
    }
}

/// システムコール呼び出しのラッパ。EINTRならリトライ
fn syscall<F, T>(f: F) -> Result<T, nix::Error>
where
    F: Fn() -> Result<T, nix::Error>,
{
    loop {
        match f() {
            Err(nix::Error::EINTR) => (),
            result => return result,
        }
    }
}

/// プロセスグループIDを指定してfork & exec
/// pgidが0の場合は子プロセスのPIDが、プロセスグループIDとなる
///
/// - inputがSome(fd)の場合は、標準入力をfdと設定
/// - outputSome(fd)の場合は、標準出力をfdと設定
fn fork_exec(
    pgid: Pid,
    filename: &str,
    args: &[&str],
    input: Option<i32>,
    output: Option<i32>,
) -> Result<Pid, DynError> {
    let filename = CString::new(filename).unwrap();
    let args: Vec<CString> = args.iter().map(|s| CString::new(*s).unwrap()).collect();

    match syscall(|| unsafe { fork() })? {
        ForkResult::Parent { child, .. } => {
            // 子プロセスのプロセスグループIDをpgidに設定
            setpgid(child, pgid).unwrap();
            Ok(child)
        }
        ForkResult::Child => {
            // 子プロセスのプロセスグループIDをpgidに設定
            setpgid(Pid::from_raw(0), pgid).unwrap();

            // 標準入出力を設定
            if let Some(infd) = input {
                syscall(|| dup2(infd, libc::STDIN_FILENO)).unwrap();
            }
            if let Some(outfd) = output {
                syscall(|| dup2(outfd, libc::STDOUT_FILENO)).unwrap();
            }

            // signal_hookで利用されるUNIXドメインソケットとpipeをクローズ
            for i in 3..=6 {
                let _ = syscall(|| unistd::close(i));
            }

            // 実行ファイルをメモリに読み込み
            match execvp(&filename, &args) {
                Err(_) => {
                    unistd::write(libc::STDERR_FILENO, "不明なコマンドを実行\n".as_bytes()).ok();
                    exit(1);
                }
                Ok(_) => unreachable!(),
            }
        }
    }
}

fn parse_cmd(line: &str) -> CmdResult {
    todo!();
}
