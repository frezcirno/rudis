mod aof;
mod config;
mod db;
mod hash;
mod list;
mod ping;
mod rdb;
mod set;
mod string;
mod unknown;
use crate::aof::{AofFsync, AofOption};
use crate::client::Client;
use crate::config::Verbosity;
use crate::frame::Frame;
use crate::rdb::AutoSave;
use crate::shared;
use aof::BgRewriteAof;
use bytes::Bytes;
use config::{ConfigGet, ConfigResetStat, ConfigRewrite, ConfigSet};
use db::{
    DbSize, Del, Exists, Expire, ExpireAt, Keys, PExpire, PExpireAt, Rename, Select, Shutdown, Type,
};
use hash::{HGet, HSet};
use list::{ListPop, ListPush};
use ping::{Echo, Ping, Quit};
use rdb::{BgSave, Save};
use set::{SAdd, SRem};
use std::io::{Error, ErrorKind, Result};
use std::vec;
use string::{Append, Get, Set, Strlen};
use string::{Decr, DecrBy, Incr, IncrBy};
use tokio::fs::File;
use unknown::Unknown;

use self::string::SetNx;

pub struct CommandParser {
    parts: vec::IntoIter<Frame>,
}

impl CommandParser {
    pub fn from(frame: Frame) -> CommandParser {
        let parts = match frame {
            Frame::Array(parts) => parts.into_iter(),
            _ => panic!("not an array frame"),
        };
        CommandParser { parts }
    }

    pub fn next(&mut self) -> Option<Frame> {
        self.parts.next()
    }

    pub fn remaining(&self) -> usize {
        self.parts.len()
    }

    pub fn has_next(&self) -> bool {
        self.remaining() > 0
    }

    pub fn next_string(&mut self) -> Result<Option<Bytes>> {
        if let Some(frame) = self.next() {
            match frame {
                Frame::Simple(s) => Ok(Some(s)),
                Frame::Bulk(b) => Ok(Some(b)),
                _ => Err(Error::new(ErrorKind::Other, "not a string")),
            }
        } else {
            Ok(None)
        }
    }

    pub fn next_integer(&mut self) -> Result<Option<i64>> {
        if let Some(frame) = self.next() {
            match frame {
                Frame::Bulk(b) => match std::str::from_utf8(&b[..]) {
                    Ok(s) => match s.parse() {
                        Ok(n) => Ok(Some(n)),
                        Err(_) => Err(Error::new(ErrorKind::Other, "not an integer")),
                    },
                    Err(_) => Err(Error::new(ErrorKind::Other, "not an integer")),
                },
                Frame::Integer(n) => Ok(Some(n)),
                _ => Err(Error::new(ErrorKind::Other, "not an integer")),
            }
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug, Clone)]
pub enum Command {
    Ping(Ping),
    Echo(Echo),
    Quit(Quit),

    Get(Get),
    Set(Set),
    SetNx(SetNx),
    Append(Append),
    Strlen(Strlen),
    Incr(Incr),
    IncrBy(IncrBy),
    Decr(Decr),
    DecrBy(DecrBy),

    Del(Del),
    Exists(Exists),
    Select(Select),
    Keys(Keys),
    DbSize(DbSize),
    Shutdown(Shutdown),
    Rename(Rename),
    Expire(Expire),
    ExpireAt(ExpireAt),
    PExpire(PExpire),
    PExpireAt(PExpireAt),
    Type(Type),

    LPush(ListPush),
    RPush(ListPush),
    LPop(ListPop),
    RPop(ListPop),

    HSet(HSet),
    HGet(HGet),

    SAdd(SAdd),
    SRem(SRem),

    Save(Save),
    BgSave(BgSave),

    BgRewriteAof(BgRewriteAof),

    ConfigGet(ConfigGet),
    ConfigSet(ConfigSet),
    ConfigResetStat(ConfigResetStat),
    ConfigRewrite(ConfigRewrite),

    // Publish(Publish),
    // Subscribe(Subscribe),
    // Unsubscribe(Unsubscribe),
    Unknown(Unknown),
}

impl Command {
    pub fn from(frame: Frame) -> Result<Command> {
        let mut parser = CommandParser::from(frame);

        let cmd = {
            let maybe_cmd = parser.next_string()?;
            if let Some(cmd) = maybe_cmd {
                cmd
            } else {
                return Err(Error::new(ErrorKind::InvalidInput, "No command provided"));
            }
        };

        let command = match &cmd.to_ascii_lowercase()[..] {
            b"ping" => Command::Ping(Ping::from(&mut parser)?),
            b"quit" => Command::Quit(Quit::from(&mut parser)?),

            b"get" => Command::Get(Get::from(&mut parser)?),
            b"set" => Command::Set(Set::from(&mut parser)?),
            b"setnx" => Command::SetNx(SetNx::from(&mut parser)?),
            b"append" => Command::Append(Append::from(&mut parser)?),
            b"strlen" => Command::Strlen(Strlen::from(&mut parser)?),
            b"incr" => Command::Incr(Incr::from(&mut parser)?),
            b"incrby" => Command::IncrBy(IncrBy::from(&mut parser)?),
            b"decr" => Command::Decr(Decr::from(&mut parser)?),
            b"decrby" => Command::DecrBy(DecrBy::from(&mut parser)?),

            b"del" => Command::Del(Del::from(&mut parser)?),
            b"exists" => Command::Exists(Exists::from(&mut parser)?),
            b"select" => Command::Select(Select::from(&mut parser)?),
            b"keys" => Command::Keys(Keys::from(&mut parser)?),
            b"dbsize" => Command::DbSize(DbSize::from(&mut parser)?),
            b"shutdown" => Command::Shutdown(Shutdown::from(&mut parser)?),
            b"rename" => Command::Rename(Rename::from(&mut parser)?),
            b"expire" => Command::Expire(Expire::from(&mut parser)?),
            b"expireat" => Command::ExpireAt(ExpireAt::from(&mut parser)?),
            b"pexpire" => Command::PExpire(PExpire::from(&mut parser)?),
            b"pexpireat" => Command::PExpireAt(PExpireAt::from(&mut parser)?),
            b"type" => Command::Type(Type::from(&mut parser)?),

            b"lpush" => Command::LPush(ListPush::from(&mut parser, true)?),
            b"rpush" => Command::RPush(ListPush::from(&mut parser, true)?),
            b"lpop" => Command::LPop(ListPop::from(&mut parser, true)?),
            b"rpop" => Command::RPop(ListPop::from(&mut parser, false)?),

            b"hset" => Command::HSet(HSet::from(&mut parser)?),
            b"hget" => Command::HGet(HGet::from(&mut parser)?),

            b"sadd" => Command::SAdd(SAdd::from(&mut parser)?),
            b"srem" => Command::SRem(SRem::from(&mut parser)?),

            b"save" => Command::Save(Save::from(&mut parser)?),
            b"bgsave" => Command::BgSave(BgSave::from(&mut parser)?),

            b"bgrewriteaof" => Command::BgRewriteAof(BgRewriteAof::from(&mut parser)?),

            b"config" => match parser.next_string()? {
                Some(subcmd) => match &subcmd.to_ascii_lowercase()[..] {
                    b"get" => Command::ConfigGet(ConfigGet::from(&mut parser)?),
                    b"set" => Command::ConfigSet(ConfigSet::from(&mut parser)?),
                    b"resetstat" => Command::ConfigResetStat(ConfigResetStat::from(&mut parser)?),
                    b"rewrite" => Command::ConfigRewrite(ConfigRewrite::from(&mut parser)?),
                    _ => Command::Unknown(Unknown::new(subcmd)),
                },
                None => {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "CONFIG subcommand not provided",
                    ))
                }
            },

            // b"publish" => Ok(Command::Publish(Publish {})),
            // b"subscribe" => Ok(Command::Subscribe(Subscribe {})),
            // b"unsubscribe" => Ok(Command::Unsubscribe(Unsubscribe {})),
            _ => Command::Unknown(Unknown::new(cmd)),
        };

        if parser.has_next() {
            return Err(Error::new(ErrorKind::Other, "Trailing bytes in the frame"));
        }

        Ok(command)
    }
}

impl Client {
    pub async fn handle_command(&mut self, cmd: Command) -> Result<()> {
        match cmd {
            Command::Select(cmd) => {
                if let Ok(()) = self.select(cmd.index as usize) {
                    self.write_frame(&shared::ok).await?;
                } else {
                    self.write_frame(&Frame::Error(Bytes::from_static(b"ERR invalid db index")))
                        .await?;
                }
            }
            Command::DbSize(_) => {
                // let len = self.dbs.len();
                let len = 1; // only one database for now
                self.write_frame(&Frame::Integer(len)).await?;
                // continue;
            }
            Command::Save(_) => {
                if self.server.rdb_state.read().await.rdb_child_pid.is_some() {
                    self.write_frame(&Frame::Error(Bytes::from_static(
                        b"ERR background save is running",
                    )))
                    .await?;
                    return Ok(());
                }
                let mut file = File::create(&self.config.read().await.rdb_filename).await?;
                self.server.save(&mut file).await?;
                self.write_frame(&shared::ok).await?;
            }
            Command::BgSave(_) => {
                if self.server.rdb_state.read().await.rdb_child_pid.is_some() {
                    self.write_frame(&Frame::Error(Bytes::from_static(
                        b"ERR background save is running",
                    )))
                    .await?;
                    return Ok(());
                }
                self.server.background_save().await?;
                self.write_frame(&shared::ok).await?;
            }
            Command::BgRewriteAof(_) => {
                if self.server.aof_state.read().await.aof_child_pid.is_some() {
                    // aof rewrite is running
                    self.write_frame(&Frame::Error(Bytes::from_static(
                        b"ERR background rewrite is running",
                    )))
                    .await?;
                } else if self.server.rdb_state.read().await.rdb_child_pid.is_some() {
                    // rdb save is running: schedule aof rewrite
                    self.server.aof_state.write().await.aof_rewrite_scheduled = true;
                    self.write_frame(&Frame::Simple(Bytes::from_static(b"BgAofRewrite schduled")))
                        .await?;
                } else {
                    let res = self
                        .server
                        // start aof rewrite in background
                        .rewrite_append_only_file_background(
                            &mut self.server.aof_state.write().await,
                        )
                        .await;
                    if res.is_err() {
                        // start aof rewrite failed
                        self.write_frame(&Frame::Error(Bytes::from_static(
                            b"ERR background rewrite error",
                        )))
                        .await?;
                    } else {
                        // start aof rewrite success
                        self.write_frame(&shared::ok).await?;
                    }
                }
            }

            Command::Ping(cmd) => cmd.apply(self).await?,
            Command::Echo(cmd) => cmd.apply(self).await?,
            Command::Quit(_) => {
                self.server.quit_ch.send(());
                self.write_frame(&shared::ok).await?;
            }

            Command::Get(cmd) => cmd.apply(self).await?,
            Command::Set(cmd) => cmd.apply(self).await?,
            Command::SetNx(cmd) => cmd.apply(self).await?,
            Command::Append(cmd) => cmd.apply(self).await?,
            Command::Strlen(cmd) => cmd.apply(self).await?,
            Command::Incr(cmd) => cmd.apply(self).await?,
            Command::IncrBy(cmd) => cmd.apply(self).await?,
            Command::Decr(cmd) => cmd.apply(self).await?,
            Command::DecrBy(cmd) => cmd.apply(self).await?,

            Command::Del(cmd) => cmd.apply(self).await?,
            Command::Exists(cmd) => cmd.apply(self).await?,
            Command::Keys(cmd) => cmd.apply(self).await?,
            Command::Shutdown(cmd) => cmd.apply(self).await?,
            Command::Rename(cmd) => cmd.apply(self).await?,
            Command::Expire(cmd) => cmd.apply(self).await?,
            Command::ExpireAt(cmd) => cmd.apply(self).await?,
            Command::PExpire(cmd) => cmd.apply(self).await?,
            Command::PExpireAt(cmd) => cmd.apply(self).await?,
            Command::Type(cmd) => cmd.apply(self).await?,

            Command::LPush(cmd) => cmd.apply(self).await?,
            Command::RPush(cmd) => cmd.apply(self).await?,
            Command::LPop(cmd) => cmd.apply(self).await?,
            Command::RPop(cmd) => cmd.apply(self).await?,

            Command::HSet(cmd) => cmd.apply(self).await?,
            Command::HGet(cmd) => cmd.apply(self).await?,

            Command::SAdd(cmd) => cmd.apply(self).await?,
            Command::SRem(cmd) => cmd.apply(self).await?,

            Command::ConfigGet(cmd) => {
                let config = self.config.clone();
                match &cmd.key[..] {
                    b"dbfilename" => {
                        self.write_frame(&Frame::Array(vec![
                            Frame::Bulk(cmd.key),
                            Frame::new_bulk_from(config.read().await.rdb_filename.clone()),
                        ]))
                        .await?;
                    }
                    b"port" => {
                        self.write_frame(&Frame::Array(vec![
                            Frame::Bulk(cmd.key),
                            Frame::new_bulk_from(config.read().await.port.to_string()),
                        ]))
                        .await?;
                    }
                    b"databases" => {
                        self.write_frame(&Frame::Array(vec![
                            Frame::Bulk(cmd.key),
                            Frame::new_bulk_from(config.read().await.db_num.to_string()),
                        ]))
                        .await?;
                    }
                    b"hz" => {
                        self.write_frame(&Frame::Array(vec![
                            Frame::Bulk(cmd.key),
                            Frame::new_bulk_from(config.read().await.hz.to_string()),
                        ]))
                        .await?;
                    }
                    b"appendonly" => {
                        self.write_frame(&Frame::Array(vec![
                            Frame::Bulk(cmd.key),
                            Frame::new_bulk_from(config.read().await.aof_state.to_string()),
                        ]))
                        .await?;
                    }
                    b"dir" => {
                        // cwd
                        let cwd = std::env::current_dir().unwrap();
                        self.write_frame(&Frame::Array(vec![
                            Frame::Bulk(cmd.key),
                            Frame::new_bulk_from_slice(cwd.to_str().unwrap().as_bytes()),
                        ]))
                        .await?;
                    }
                    b"appendfsync" => {
                        self.write_frame(&Frame::Array(vec![
                            Frame::Bulk(cmd.key),
                            Frame::new_bulk_from(config.read().await.aof_fsync.to_string()),
                        ]))
                        .await?;
                    }
                    b"save" => {
                        let save_params = self
                            .config
                            .read()
                            .await
                            .save_params
                            .iter()
                            .map(|save| save.to_string())
                            .collect::<Vec<String>>()
                            .join(" ");

                        self.write_frame(&Frame::Array(vec![
                            Frame::Bulk(cmd.key),
                            Frame::new_bulk_from(save_params),
                        ]))
                        .await?;
                    }
                    b"loglevel" => {
                        self.write_frame(&Frame::Array(vec![
                            Frame::Bulk(cmd.key),
                            Frame::new_bulk_from(config.read().await.verbosity.to_string()),
                        ]))
                        .await?;
                    }
                    b"bind" => {
                        self.write_frame(&Frame::Array(vec![
                            Frame::Bulk(cmd.key),
                            Frame::new_bulk_from_slice(config.read().await.bindaddr.as_bytes()),
                        ]))
                        .await?;
                    }
                    _ => {
                        self.write_frame(&Frame::Error(Bytes::from_static(
                            b"ERR no such configuration",
                        )))
                        .await?;
                    }
                }
            }
            Command::ConfigSet(cmd) => match &cmd.key[..] {
                b"dbfilename" => match String::from_utf8(cmd.value.to_vec()) {
                    Ok(newval) => {
                        self.config.write().await.rdb_filename = newval;
                        self.write_frame(&shared::ok).await?;
                    }
                    Err(_) => {
                        self.write_frame(&Frame::Error(Bytes::from_static(
                            b"ERR invalid dbfilename",
                        )))
                        .await?;
                        return Ok(());
                    }
                },
                b"port" => {
                    if let Ok(port) = std::str::from_utf8(&cmd.value).unwrap().parse::<u16>() {
                        self.config.write().await.port = port;
                        self.write_frame(&shared::ok).await?;
                    } else {
                        self.write_frame(&Frame::Error(Bytes::from_static(b"ERR invalid port")))
                            .await?;
                    }
                }
                b"databases" => {
                    if let Ok(db_num) = std::str::from_utf8(&cmd.value).unwrap().parse::<usize>() {
                        self.config.write().await.db_num = db_num;
                        self.write_frame(&shared::ok).await?;
                    } else {
                        self.write_frame(&Frame::Error(Bytes::from_static(b"ERR invalid db_num")))
                            .await?;
                    }
                }
                b"hz" => {
                    if let Ok(hz) = std::str::from_utf8(&cmd.value).unwrap().parse::<usize>() {
                        self.config.write().await.hz = hz;
                        self.write_frame(&shared::ok).await?;
                    } else {
                        self.write_frame(&Frame::Error(Bytes::from_static(b"ERR invalid hz")))
                            .await?;
                    }
                }
                b"appendonly" => {
                    let aof_state = match std::str::from_utf8(&cmd.value).unwrap() {
                        "on" => AofOption::On,
                        "off" => AofOption::Off,
                        _ => {
                            self.write_frame(&Frame::Error(Bytes::from_static(
                                b"ERR invalid appendonly",
                            )))
                            .await?;
                            return Ok(());
                        }
                    };
                    self.config.write().await.aof_state = aof_state;
                    self.write_frame(&shared::ok).await?;
                }
                b"appendfsync" => {
                    let aof_fsync = match std::str::from_utf8(&cmd.value).unwrap() {
                        "everysec" => AofFsync::Everysec,
                        "always" => AofFsync::Always,
                        "no" => AofFsync::No,
                        _ => {
                            self.write_frame(&Frame::Error(Bytes::from_static(
                                b"ERR invalid appendfsync",
                            )))
                            .await?;
                            return Ok(());
                        }
                    };
                    self.config.write().await.aof_fsync = aof_fsync;
                    self.write_frame(&shared::ok).await?;
                }
                b"save" => {
                    let save_params = std::str::from_utf8(&cmd.value).unwrap();
                    let save_params = save_params
                        .split_whitespace()
                        .map(|param| {
                            let mut iter = param.split_whitespace();
                            let seconds = iter.next().unwrap().parse().unwrap();
                            let changes = iter.next().unwrap().parse().unwrap();
                            AutoSave { seconds, changes }
                        })
                        .collect();
                    self.config.write().await.save_params = save_params;
                    self.write_frame(&shared::ok).await?;
                }
                b"dir" => {
                    // chdir
                    let dir = std::str::from_utf8(&cmd.value).unwrap();
                    if let Ok(_) = std::env::set_current_dir(dir) {
                        self.write_frame(&shared::ok).await?;
                    } else {
                        self.write_frame(&Frame::Error(Bytes::from_static(b"ERR invalid dir")))
                            .await?;
                    }
                }
                b"loglevel" => {
                    let verbosity = match std::str::from_utf8(&cmd.value).unwrap() {
                        "quiet" => Verbosity::Quiet,
                        "normal" => Verbosity::Normal,
                        "verbose" => Verbosity::Verbose,
                        "debug" => Verbosity::Debug,
                        _ => {
                            self.write_frame(&Frame::Error(Bytes::from_static(
                                b"ERR invalid loglevel",
                            )))
                            .await?;
                            return Ok(());
                        }
                    };
                    self.config.write().await.verbosity = verbosity;
                    self.write_frame(&shared::ok).await?;
                }
                _ => {
                    self.write_frame(&Frame::Error(Bytes::from_static(
                        b"ERR no such configuration",
                    )))
                    .await?;
                }
            },
            Command::ConfigResetStat(_) => todo!(),
            Command::ConfigRewrite(_) => todo!(),

            // Publish(cmd) => cmd.apply(self).await?,
            // Subscribe(cmd) => cmd.apply(db, dst, shutdown).await,
            // `Unsubscribe` cannot be applied. It may only be received from the
            // context of a `Subscribe` command.
            // Unsubscribe(_) => Err("`Unsubscribe` is unsupported in this context".into()),
            Command::Unknown(cmd) => cmd.apply(self).await?,
        };

        Ok(())
    }
}
