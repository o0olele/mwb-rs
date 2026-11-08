use super::wire::{Command, Reply, Request, VERSION};
use crate::{
    layout::Point,
    platform::InputInjectionError,
    protocol::{InputEvent, MouseMode},
};
use anyhow::{Result, ensure};

pub(super) trait Backend {
    /// Return true after moving to a different input desktop. Release all held
    /// input on the previous desktop before detaching from it.
    fn refresh_desktop(&mut self) -> Result<bool>;
    /// A failed positioning injection must preserve its InputInjectionError.
    fn enter(&mut self, point: Point, mode: MouseMode) -> Result<()>;
    // Preserve InputInjectionError so only a confirmed zero-insertion failure
    // can be retried when a desktop switch races the initial readiness check.
    fn apply(&mut self, event: InputEvent, keyboard: bool) -> Result<()>;
    fn position(&mut self) -> Result<Point>;
    fn release(&mut self);
    fn taken_over(&mut self) -> bool;
}

pub(super) struct Session<B: Backend> {
    pub backend: B,
    sequence: u64,
    ready: bool,
    mode: Option<MouseMode>,
    point: Point,
    taken_over: bool,
}

impl<B: Backend> Session<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            sequence: 0,
            ready: false,
            mode: None,
            point: Point::default(),
            taken_over: false,
        }
    }

    pub fn handle(&mut self, request: Request) -> Result<Reply> {
        ensure!(
            request.version == VERSION,
            "锁屏输入服务版本不同，请重新安装配套版本"
        );
        ensure!(
            self.sequence.checked_add(1) == Some(request.sequence),
            "输入服务消息序号无效"
        );
        self.sequence = request.sequence;
        if matches!(request.command, Command::Hello) {
            ensure!(!self.ready, "重复的输入服务握手");
            self.ready = true;
            return Ok(Reply::Ok);
        }
        ensure!(self.ready, "输入服务尚未握手");
        if self.backend.taken_over() && self.mode.is_some() {
            self.release();
            self.taken_over = true;
        }
        match request.command {
            Command::Hello => unreachable!(),
            Command::Release => {
                self.release();
                self.taken_over = false;
                Ok(Reply::Ok)
            }
            Command::Enter { point, mode } => {
                ensure!(
                    point.x.is_finite() && point.y.is_finite(),
                    "输入服务坐标无效"
                );
                self.release();
                self.backend.refresh_desktop()?;
                self.enter_current_desktop(point, mode)?;
                self.mode = Some(mode);
                self.point = point;
                self.taken_over = false;
                Ok(Reply::Ok)
            }
            Command::Input { event, keyboard } => {
                if self.taken_over {
                    return Ok(Reply::TakenOver);
                }
                self.refresh()?;
                if let Err(error) = self.backend.apply(event.clone(), keyboard) {
                    if !nothing_inserted(&error) || !self.refresh()? {
                        return Err(error);
                    }
                    self.backend.apply(event.clone(), keyboard)?;
                }
                if let InputEvent::Absolute { x, y } = event {
                    self.point = Point {
                        x: x as f64,
                        y: y as f64,
                    };
                }
                // A successful Enter key can immediately unlock Windows.
                // Do not turn a subsequent cursor-read race into an injection
                // failure or resend a successfully inserted key.
                Ok(Reply::Ok)
            }
            Command::Position => {
                if self.taken_over {
                    return Ok(Reply::TakenOver);
                }
                self.refresh()?;
                self.point = match self.backend.position() {
                    Ok(point) => point,
                    Err(error) => {
                        if !self.refresh()? {
                            return Err(error);
                        }
                        self.backend.position()?
                    }
                };
                Ok(Reply::Position(self.point))
            }
        }
    }

    fn enter_current_desktop(&mut self, point: Point, mode: MouseMode) -> Result<()> {
        if let Err(error) = self.backend.enter(point, mode) {
            // Enter also injects a mouse event. Lock/unlock can race this first
            // positioning call just as it can race a later key or mouse event.
            if !nothing_inserted(&error) || !self.backend.refresh_desktop()? {
                return Err(error);
            }
            self.backend.enter(point, mode)?;
        }
        Ok(())
    }

    fn refresh(&mut self) -> Result<bool> {
        let mode = self
            .mode
            .ok_or_else(|| anyhow::anyhow!("输入服务没有活动控制会话"))?;
        let changed = self.backend.refresh_desktop()?;
        if changed {
            // The old desktop's key/button state is never replayed onto a login
            // screen. Preserve only mouse mode/position for continuous control.
            self.enter_current_desktop(self.point, mode)?;
        }
        Ok(changed)
    }

    fn release(&mut self) {
        self.mode = None;
        self.backend.release();
    }
}

fn nothing_inserted(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<InputInjectionError>()
        .is_some_and(InputInjectionError::nothing_inserted)
}

impl<B: Backend> Drop for Session<B> {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    #[derive(Default)]
    struct Fake {
        changed: bool,
        enter_failures: VecDeque<(bool, anyhow::Error)>,
        apply_failures: VecDeque<(bool, anyhow::Error)>,
        switch_on_position: bool,
        switch_after_apply: bool,
        enter_attempts: usize,
        apply_attempts: usize,
        applies: usize,
        takeover: bool,
        enters: usize,
        held: bool,
        releases: Arc<AtomicUsize>,
        mode: Option<MouseMode>,
    }
    impl Backend for Fake {
        fn refresh_desktop(&mut self) -> Result<bool> {
            let changed = std::mem::take(&mut self.changed);
            if changed {
                self.release();
            }
            Ok(changed)
        }
        fn enter(&mut self, _: Point, mode: MouseMode) -> Result<()> {
            self.enter_attempts += 1;
            if let Some((changed, error)) = self.enter_failures.pop_front() {
                self.changed |= changed;
                return Err(error);
            }
            self.enters += 1;
            self.mode = Some(mode);
            Ok(())
        }
        fn apply(&mut self, event: InputEvent, _: bool) -> Result<()> {
            self.apply_attempts += 1;
            if let Some((changed, error)) = self.apply_failures.pop_front() {
                self.changed |= changed;
                return Err(error);
            }
            self.changed |= std::mem::take(&mut self.switch_after_apply);
            self.applies += 1;
            if let InputEvent::Key { down, .. } = event {
                self.held = down;
            }
            Ok(())
        }
        fn position(&mut self) -> Result<Point> {
            if std::mem::take(&mut self.switch_on_position) {
                self.changed = true;
                anyhow::bail!("desktop changed before GetPhysicalCursorPos");
            }
            Ok(Point { x: 20.0, y: 30.0 })
        }
        fn release(&mut self) {
            self.held = false;
            self.mode = None;
            self.releases.fetch_add(1, Ordering::SeqCst);
        }
        fn taken_over(&mut self) -> bool {
            std::mem::take(&mut self.takeover)
        }
    }
    fn request(session: &mut Session<Fake>, command: Command) -> Result<Reply> {
        session.handle(Request {
            version: VERSION,
            sequence: session.sequence + 1,
            command,
        })
    }
    fn send(session: &mut Session<Fake>, command: Command) -> Reply {
        request(session, command).unwrap()
    }
    fn rejected(kind: &'static str, inserted: u32, code: u32) -> anyhow::Error {
        anyhow::Error::new(InputInjectionError::new(kind, inserted, code))
            .context("input agent injection failed")
    }
    fn key() -> Command {
        Command::Input {
            event: InputEvent::Key {
                vk: 65,
                scan: 30,
                extended: false,
                down: true,
            },
            keyboard: true,
        }
    }
    fn enter(mode: MouseMode) -> Command {
        Command::Enter {
            point: Point { x: 20.0, y: 30.0 },
            mode,
        }
    }

    #[test]
    fn lock_and_unlock_keep_mouse_mode_but_release_keys_on_each_desktop() {
        for mode in [MouseMode::Relative, MouseMode::ScreenRelative] {
            let mut session = Session::new(Fake::default());
            send(&mut session, Command::Hello);
            send(&mut session, enter(mode));
            for _ in 0..2 {
                send(&mut session, key());
                assert!(session.backend.held);
                session.backend.changed = true;
                assert!(matches!(
                    send(&mut session, Command::Position),
                    Reply::Position(_)
                ));
                assert!(!session.backend.held);
                assert_eq!(session.backend.mode, Some(mode));
            }
            assert_eq!(session.backend.enters, 3);
        }
    }

    #[test]
    fn physical_takeover_rejects_queued_keys_until_a_new_control_session() {
        let mut session = Session::new(Fake::default());
        send(&mut session, Command::Hello);
        send(&mut session, enter(MouseMode::Relative));
        send(&mut session, key());
        session.backend.takeover = true;
        assert!(matches!(send(&mut session, key()), Reply::TakenOver));
        assert!(!session.backend.held);
        assert!(matches!(
            send(&mut session, Command::Position),
            Reply::TakenOver
        ));
        send(&mut session, enter(MouseMode::Relative));
        send(&mut session, key());
        assert!(session.backend.held);
        let releases = session.backend.releases.clone();
        let before = releases.load(Ordering::SeqCst);
        drop(session);
        assert!(releases.load(Ordering::SeqCst) > before);
    }

    #[test]
    fn desktop_switch_races_retry_only_uninserted_input_and_cursor_reads() {
        let mut session = Session::new(Fake::default());
        send(&mut session, Command::Hello);
        send(&mut session, enter(MouseMode::Relative));
        session
            .backend
            .apply_failures
            .push_back((true, rejected("键盘", 0, 5)));
        send(&mut session, key());
        assert_eq!(session.backend.applies, 1);
        session.backend.switch_on_position = true;
        // A successful key is acknowledged without a potentially failing read.
        send(&mut session, key());
        assert_eq!(session.backend.applies, 2);
        assert!(matches!(
            send(&mut session, Command::Position),
            Reply::Position(_)
        ));
        assert_eq!(session.backend.applies, 2);
        assert_eq!(session.backend.enters, 3);
    }

    #[test]
    fn first_position_retries_once_only_when_zero_insertion_races_a_desktop_switch() {
        for mode in [MouseMode::Relative, MouseMode::ScreenRelative] {
            for code in [0, 5] {
                let mut session = Session::new(Fake::default());
                send(&mut session, Command::Hello);
                session
                    .backend
                    .enter_failures
                    .push_back((true, rejected("鼠标", 0, code)));
                send(&mut session, enter(mode));
                assert_eq!(session.backend.enter_attempts, 2);
                assert_eq!(session.backend.enters, 1);
                assert_eq!(session.backend.mode, Some(mode));
                send(&mut session, key());
                assert_eq!(session.backend.applies, 1);
            }
        }
    }

    #[test]
    fn first_position_failure_on_an_unchanged_desktop_is_not_retried() {
        let mut session = Session::new(Fake::default());
        send(&mut session, Command::Hello);
        session
            .backend
            .enter_failures
            .push_back((false, rejected("鼠标", 0, 5)));
        assert!(request(&mut session, enter(MouseMode::Relative)).is_err());
        assert_eq!(session.backend.enter_attempts, 1);
        assert!(session.mode.is_none());
    }

    #[test]
    fn repeated_desktop_switches_do_not_cause_unbounded_entry_retries() {
        let mut session = Session::new(Fake::default());
        send(&mut session, Command::Hello);
        for _ in 0..2 {
            session
                .backend
                .enter_failures
                .push_back((true, rejected("鼠标", 0, 5)));
        }
        assert!(request(&mut session, enter(MouseMode::Relative)).is_err());
        assert_eq!(session.backend.enter_attempts, 2);
        assert!(session.mode.is_none());
    }

    #[test]
    fn unknown_or_nonzero_insertion_failures_are_not_retried_after_a_switch() {
        for entering in [false, true] {
            for error in [
                anyhow::anyhow!("unconfirmed result"),
                rejected("键盘", 1, 5),
            ] {
                let mut session = Session::new(Fake::default());
                send(&mut session, Command::Hello);
                if entering {
                    session.backend.enter_failures.push_back((true, error));
                    assert!(request(&mut session, enter(MouseMode::Relative)).is_err());
                    assert_eq!(session.backend.enter_attempts, 1);
                } else {
                    send(&mut session, enter(MouseMode::Relative));
                    session.backend.apply_failures.push_back((true, error));
                    assert!(request(&mut session, key()).is_err());
                    assert_eq!(session.backend.apply_attempts, 1);
                }
            }
        }
    }

    #[test]
    fn successful_unlock_key_is_not_replayed_when_the_desktop_changes_immediately() {
        let mut session = Session::new(Fake::default());
        send(&mut session, Command::Hello);
        send(&mut session, enter(MouseMode::Relative));
        session.backend.switch_after_apply = true;
        send(&mut session, key());
        send(&mut session, Command::Position);
        assert_eq!(session.backend.apply_attempts, 1);
        assert_eq!(session.backend.applies, 1);
        assert!(!session.backend.held);
    }

    #[test]
    fn legacy_client_cannot_handshake_with_ui_access_worker() {
        for version in [1, 2] {
            let mut session = Session::new(Fake::default());
            let error = session
                .handle(Request {
                    version,
                    sequence: 1,
                    command: Command::Hello,
                })
                .unwrap_err();
            assert!(error.to_string().contains("重新安装"));
            assert!(!session.ready);
            assert_eq!(session.backend.enter_attempts, 0);
            assert_eq!(session.backend.apply_attempts, 0);
        }
    }

    #[test]
    fn protocol_rejects_input_before_enter_and_replayed_sequences() {
        let mut session = Session::new(Fake::default());
        assert!(
            session
                .handle(Request {
                    version: VERSION,
                    sequence: 1,
                    command: key()
                })
                .is_err()
        );
        let mut session = Session::new(Fake::default());
        send(&mut session, Command::Hello);
        assert!(
            session
                .handle(Request {
                    version: VERSION,
                    sequence: 1,
                    command: enter(MouseMode::Relative)
                })
                .is_err()
        );
        assert!(
            session
                .handle(Request {
                    version: VERSION,
                    sequence: 2,
                    command: key()
                })
                .is_err()
        );
        assert!(
            session
                .handle(Request {
                    version: VERSION + 1,
                    sequence: 3,
                    command: Command::Hello
                })
                .is_err()
        );
    }
}
