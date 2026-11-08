use super::InputInjectionError;
use crate::{
    layout::Point,
    protocol::{InputEvent, MouseMode},
};
use anyhow::{Context, Result};

pub(super) trait Backend {
    fn enter(&mut self, point: Point, mode: MouseMode) -> Result<()>;
    fn apply(&mut self, event: InputEvent, keyboard: bool) -> Result<()>;
    fn position(&mut self) -> Result<Point>;
    fn release(&mut self);
}

pub(super) trait Environment {
    type Service: Backend;
    fn native_available(&mut self) -> Result<bool>;
    fn connect_service(&mut self) -> Result<Self::Service>;
}

/// Successful ordinary desktop input never needs the optional service.
/// Once a control session moves to the service, keep it there until release:
/// a failed service reply may follow an already-inserted event.
pub(super) struct Receiver<N: Backend, E: Environment> {
    native: N,
    environment: E,
    service: Option<E::Service>,
    mode: Option<MouseMode>,
    point: Point,
}

impl<N: Backend, E: Environment> Receiver<N, E> {
    pub fn new(native: N, environment: E) -> Self {
        Self {
            native,
            environment,
            service: None,
            mode: None,
            point: Point::default(),
        }
    }

    pub fn enter(&mut self, point: Point, mode: MouseMode) -> Result<()> {
        self.release();
        let result = if self.environment.native_available()? {
            match self.native.enter(point, mode) {
                Err(error) if nothing_inserted(&error) => self
                    .connect_and_enter(point, mode)
                    .with_context(|| format!("普通桌面定位未插入事件：{error:#}")),
                result => result,
            }
        } else {
            self.connect_and_enter(point, mode)
        };
        match result {
            Ok(()) => {
                self.point = point;
                self.mode = Some(mode);
                Ok(())
            }
            Err(error) => {
                self.release();
                Err(error)
            }
        }
    }

    fn connect_and_enter(&mut self, point: Point, mode: MouseMode) -> Result<()> {
        self.native.release();
        let mut service = self.environment.connect_service()?;
        service.enter(point, mode)?;
        self.service = Some(service);
        Ok(())
    }

    fn follow_desktop(&mut self) -> Result<()> {
        if self.service.is_none() && !self.environment.native_available()? {
            let mode = self.mode.context("没有活动输入会话")?;
            self.connect_and_enter(self.point, mode)?;
        }
        Ok(())
    }

    pub fn apply(&mut self, event: InputEvent, keyboard: bool) -> Result<()> {
        self.follow_desktop()?;
        if let Some(service) = self.service.as_mut() {
            service.apply(event.clone(), keyboard)?;
        } else if let Err(error) = self.native.apply(event.clone(), keyboard) {
            if !nothing_inserted(&error) {
                return Err(error);
            }
            // An active desktop is not proof that this process can inject into
            // it (UIPI and login transitions can reject SendInput). Promote only
            // an explicitly uninserted event, even if UOI_IO is still true.
            let mode = self.mode.context("没有活动输入会话")?;
            self.connect_and_enter(self.point, mode)
                .with_context(|| format!("普通桌面输入未插入事件：{error:#}"))?;
            self.service
                .as_mut()
                .context("锁屏输入服务未连接")?
                .apply(event.clone(), keyboard)?;
        }
        if let InputEvent::Absolute { x, y } = event {
            self.point = Point {
                x: x as f64,
                y: y as f64,
            };
        }
        Ok(())
    }

    pub fn position(&mut self) -> Result<Point> {
        self.follow_desktop()?;
        let point = if let Some(service) = self.service.as_mut() {
            service.position()?
        } else {
            match self.native.position() {
                Ok(point) => point,
                Err(error) => {
                    if self.environment.native_available()? {
                        return Err(error);
                    }
                    self.follow_desktop()?;
                    self.service
                        .as_mut()
                        .context("锁屏输入服务未连接")?
                        .position()?
                }
            }
        };
        self.point = point;
        Ok(point)
    }

    pub fn release(&mut self) {
        self.mode = None;
        if let Some(mut service) = self.service.take() {
            service.release();
        }
        self.native.release();
    }
}

fn nothing_inserted(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<InputInjectionError>()
        .is_some_and(InputInjectionError::nothing_inserted)
}

impl<N: Backend + Default, E: Environment + Default> Default for Receiver<N, E> {
    fn default() -> Self {
        Self::new(N::default(), E::default())
    }
}
impl<N: Backend, E: Environment> Drop for Receiver<N, E> {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::InputServiceError;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    #[derive(Default)]
    struct Probe {
        native_available: AtomicBool,
        deny_service: AtomicBool,
        fail_service_after_input: AtomicBool,
        switch_during_native_input: AtomicBool,
        switch_during_native_enter: AtomicBool,
        native_error: Mutex<Option<anyhow::Error>>,
        connects: AtomicUsize,
        native_enters: AtomicUsize,
        native_releases: AtomicUsize,
        native_events: Mutex<Vec<InputEvent>>,
        service_enters: Mutex<Vec<(Point, MouseMode)>>,
        service_events: Mutex<Vec<InputEvent>>,
    }
    struct FakeNative(Arc<Probe>);
    struct FakeService(Arc<Probe>);
    struct FakeEnvironment(Arc<Probe>);
    impl Environment for FakeEnvironment {
        type Service = FakeService;
        fn native_available(&mut self) -> Result<bool> {
            Ok(self.0.native_available.load(Ordering::SeqCst))
        }
        fn connect_service(&mut self) -> Result<FakeService> {
            self.0.connects.fetch_add(1, Ordering::SeqCst);
            if self.0.deny_service.load(Ordering::SeqCst) {
                return Err(InputServiceError("拒绝访问".into()).into());
            }
            Ok(FakeService(self.0.clone()))
        }
    }
    impl Backend for FakeNative {
        fn enter(&mut self, _: Point, _: MouseMode) -> Result<()> {
            if self
                .0
                .switch_during_native_enter
                .swap(false, Ordering::SeqCst)
            {
                self.0.native_available.store(false, Ordering::SeqCst);
                return Err(InputInjectionError::new("鼠标", 0, 5).into());
            }
            if let Some(error) = self.0.native_error.lock().unwrap().take() {
                return Err(error);
            }
            self.0.native_enters.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn apply(&mut self, event: InputEvent, _: bool) -> Result<()> {
            if self
                .0
                .switch_during_native_input
                .swap(false, Ordering::SeqCst)
            {
                self.0.native_available.store(false, Ordering::SeqCst);
                return Err(InputInjectionError::new("键盘", 0, 5).into());
            }
            if let Some(error) = self.0.native_error.lock().unwrap().take() {
                return Err(error);
            }
            self.0.native_events.lock().unwrap().push(event);
            Ok(())
        }
        fn position(&mut self) -> Result<Point> {
            Ok(Point { x: 40.0, y: 50.0 })
        }
        fn release(&mut self) {
            self.0.native_releases.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Backend for FakeService {
        fn enter(&mut self, point: Point, mode: MouseMode) -> Result<()> {
            self.0.service_enters.lock().unwrap().push((point, mode));
            Ok(())
        }
        fn apply(&mut self, event: InputEvent, _: bool) -> Result<()> {
            self.0.service_events.lock().unwrap().push(event);
            if self.0.fail_service_after_input.load(Ordering::SeqCst) {
                return Err(InputServiceError("服务已插入输入但确认丢失".into()).into());
            }
            Ok(())
        }
        fn position(&mut self) -> Result<Point> {
            Ok(Point { x: 40.0, y: 50.0 })
        }
        fn release(&mut self) {}
    }
    fn key() -> InputEvent {
        InputEvent::Key {
            vk: 65,
            scan: 30,
            extended: false,
            down: true,
        }
    }
    fn receiver(native_available: bool) -> (Receiver<FakeNative, FakeEnvironment>, Arc<Probe>) {
        let probe = Arc::new(Probe::default());
        probe
            .native_available
            .store(native_available, Ordering::SeqCst);
        (
            Receiver::new(FakeNative(probe.clone()), FakeEnvironment(probe.clone())),
            probe,
        )
    }

    #[test]
    fn ordinary_desktop_never_requires_access_to_the_optional_service() {
        for mode in [MouseMode::Relative, MouseMode::ScreenRelative] {
            let (mut input, probe) = receiver(true);
            probe.deny_service.store(true, Ordering::SeqCst);
            input.enter(Point::default(), mode).unwrap();
            input.apply(key(), true).unwrap();
            input.position().unwrap();
            assert_eq!(probe.connects.load(Ordering::SeqCst), 0);
            assert_eq!(probe.native_enters.load(Ordering::SeqCst), 1);
            assert_eq!(*probe.native_events.lock().unwrap(), vec![key()]);
        }
    }

    #[test]
    fn lock_migrates_mode_and_last_position_once_and_keeps_service_until_release() {
        for mode in [MouseMode::Relative, MouseMode::ScreenRelative] {
            let (mut input, probe) = receiver(true);
            input.enter(Point::default(), mode).unwrap();
            let point = input.position().unwrap();
            let before = probe.native_releases.load(Ordering::SeqCst);
            probe.native_available.store(false, Ordering::SeqCst);
            input.position().unwrap();
            assert!(probe.native_releases.load(Ordering::SeqCst) > before);
            assert_eq!(*probe.service_enters.lock().unwrap(), vec![(point, mode)]);
            probe.native_available.store(true, Ordering::SeqCst);
            input.apply(key(), true).unwrap();
            assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
            assert!(probe.native_events.lock().unwrap().is_empty());
            input.release();
            input.enter(point, mode).unwrap();
            input.apply(key(), true).unwrap();
            assert_eq!(*probe.native_events.lock().unwrap(), vec![key()]);
        }
    }

    #[test]
    fn missing_secure_desktop_permission_is_reported_without_faking_native_success() {
        let (mut input, probe) = receiver(false);
        probe.deny_service.store(true, Ordering::SeqCst);
        let error = input
            .enter(Point::default(), MouseMode::Relative)
            .unwrap_err();
        assert!(error.is::<InputServiceError>());
        assert_eq!(probe.native_enters.load(Ordering::SeqCst), 0);
        assert!(probe.native_events.lock().unwrap().is_empty());
    }

    #[test]
    fn native_switch_race_retries_only_the_uninserted_event() {
        let (mut input, probe) = receiver(true);
        input.enter(Point::default(), MouseMode::Relative).unwrap();
        probe
            .switch_during_native_input
            .store(true, Ordering::SeqCst);
        input.apply(key(), true).unwrap();
        assert!(probe.native_events.lock().unwrap().is_empty());
        assert_eq!(*probe.service_events.lock().unwrap(), vec![key()]);
    }

    #[test]
    fn desktop_switch_during_entry_selects_service_before_accepting_control() {
        let (mut input, probe) = receiver(true);
        probe
            .switch_during_native_enter
            .store(true, Ordering::SeqCst);
        let point = Point { x: 10.0, y: 20.0 };
        input.enter(point, MouseMode::ScreenRelative).unwrap();
        assert_eq!(probe.native_enters.load(Ordering::SeqCst), 0);
        assert_eq!(
            *probe.service_enters.lock().unwrap(),
            vec![(point, MouseMode::ScreenRelative)]
        );
    }

    #[test]
    fn rejected_native_input_migrates_even_while_the_desktop_reports_active() {
        for mode in [MouseMode::Relative, MouseMode::ScreenRelative] {
            for code in [0, 5] {
                let (mut input, probe) = receiver(true);
                input.enter(Point::default(), mode).unwrap();
                let point = input.position().unwrap();
                *probe.native_error.lock().unwrap() = Some(
                    anyhow::Error::new(InputInjectionError::new("键盘", 0, code))
                        .context("SendInput rejected on an active desktop"),
                );
                input.apply(key(), true).unwrap();
                assert!(probe.native_available.load(Ordering::SeqCst));
                assert_eq!(probe.connects.load(Ordering::SeqCst), 1);
                assert_eq!(*probe.service_enters.lock().unwrap(), vec![(point, mode)]);
                assert_eq!(*probe.service_events.lock().unwrap(), vec![key()]);
                assert!(probe.native_events.lock().unwrap().is_empty());
            }
        }
    }

    #[test]
    fn rejected_first_position_migrates_even_while_the_desktop_reports_active() {
        let point = Point { x: 10.0, y: 20.0 };
        for code in [0, 5] {
            let (mut input, probe) = receiver(true);
            *probe.native_error.lock().unwrap() =
                Some(InputInjectionError::new("鼠标", 0, code).into());
            input.enter(point, MouseMode::ScreenRelative).unwrap();
            assert_eq!(probe.native_enters.load(Ordering::SeqCst), 0);
            assert_eq!(
                *probe.service_enters.lock().unwrap(),
                vec![(point, MouseMode::ScreenRelative)]
            );
        }
    }

    #[test]
    fn unconfirmed_native_errors_are_never_replayed_through_the_service() {
        for error in [
            anyhow::anyhow!("unknown input result"),
            InputInjectionError::new("键盘", 1, 5).into(),
        ] {
            let (mut input, probe) = receiver(true);
            input.enter(Point::default(), MouseMode::Relative).unwrap();
            *probe.native_error.lock().unwrap() = Some(error);
            assert!(input.apply(key(), true).is_err());
            assert_eq!(probe.connects.load(Ordering::SeqCst), 0);
            assert!(probe.service_events.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn lost_service_ack_never_replays_input_on_the_native_desktop() {
        let (mut input, probe) = receiver(false);
        input.enter(Point::default(), MouseMode::Relative).unwrap();
        probe.native_available.store(true, Ordering::SeqCst);
        probe.fail_service_after_input.store(true, Ordering::SeqCst);
        assert!(
            input
                .apply(key(), true)
                .unwrap_err()
                .is::<InputServiceError>()
        );
        assert!(probe.native_events.lock().unwrap().is_empty());
        assert_eq!(*probe.service_events.lock().unwrap(), vec![key()]);
    }
}
