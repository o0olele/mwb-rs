use super::{
    SERVICE_EXE,
    identity::{self, VerifiedWorker},
    pipe::{Pipe, TIMEOUT},
    pipe_name,
    wire::{Command, Reply, Request, Response, VERSION},
};
use crate::{
    layout::Point,
    platform::{InputServiceError, LocalInputTakeover},
    protocol::{InputEvent, MouseMode},
};
use anyhow::{Context, Result, ensure};

pub(crate) struct Client {
    pipe: Pipe,
    _server: VerifiedWorker,
    sequence: u64,
    failed: bool,
}

impl Client {
    pub fn connect() -> Result<Option<Self>> {
        let session = identity::current_session().context("读取当前 Windows 会话")?;
        let Some(pipe) = Pipe::open(&pipe_name(session)).context("打开输入服务管道")?
        else {
            return Ok(None);
        };
        let server =
            identity::verify_worker(pipe.handle.0, session, &identity::sibling(SERVICE_EXE)?)?;
        let mut client = Self {
            pipe,
            _server: server,
            sequence: 0,
            failed: false,
        };
        client.ok(Command::Hello).context("与输入服务握手")?;
        Ok(Some(client))
    }

    fn request(&mut self, command: Command) -> Result<Reply> {
        let result = (|| {
            ensure!(!self.failed, "锁屏输入服务连接已失效");
            self.sequence = self.sequence.checked_add(1).context("输入服务序号耗尽")?;
            self.pipe.send(&Request {
                version: VERSION,
                sequence: self.sequence,
                command,
            })?;
            let response: Response = self.pipe.receive(TIMEOUT)?;
            response.validate(self.sequence)?;
            match response.result {
                Reply::Error(error) => anyhow::bail!("{error}"),
                reply => Ok(reply),
            }
        })();
        match result {
            Ok(Reply::TakenOver) => Err(LocalInputTakeover.into()),
            Ok(reply) => Ok(reply),
            Err(error) => {
                self.failed = true;
                Err(InputServiceError(format!("锁屏输入服务不可用：{error:#}")).into())
            }
        }
    }

    fn ok(&mut self, command: Command) -> Result<()> {
        if !matches!(self.request(command)?, Reply::Ok) {
            self.failed = true;
            return Err(InputServiceError("输入服务响应类型无效".into()).into());
        }
        Ok(())
    }
    pub fn enter(&mut self, point: Point, mode: MouseMode) -> Result<()> {
        self.ok(Command::Enter { point, mode })
    }
    pub fn apply(&mut self, event: InputEvent, keyboard: bool) -> Result<()> {
        self.ok(Command::Input { event, keyboard })
    }
    pub fn position(&mut self) -> Result<Point> {
        match self.request(Command::Position)? {
            Reply::Position(point) => Ok(point),
            _ => {
                self.failed = true;
                Err(InputServiceError("输入服务光标响应无效".into()).into())
            }
        }
    }
    pub fn release(&mut self) {
        if !self.failed {
            let _ = self.ok(Command::Release);
        }
    }
}
