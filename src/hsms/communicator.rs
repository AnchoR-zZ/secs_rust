use crate::hsms::manager::HsmsManager;
use crate::hsms::message::HsmsMessage;
use crate::hsms::{ConnectionState, HsmsCommand, HsmsError, config::HsmsConfig};
use crate::util::SystemBytesGenerator;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Clone)]
pub struct HsmsCommunicator {
    // 发送命令给管理器的通道
    to_manager_cmd_tx: mpsc::Sender<HsmsCommand>,
    // 实时监控连接状态
    from_manager_state_rx: watch::Receiver<ConnectionState>,
    system_bytes: Arc<SystemBytesGenerator>,
}

impl HsmsCommunicator {
    pub fn new(config: HsmsConfig) -> (Self, mpsc::Receiver<HsmsMessage>) {
        // 命令通道：发送给下层管理器
        let (to_manager_cmd_tx, from_communicator_cmd_rx) = mpsc::channel::<HsmsCommand>(32);
        // 消息通道：接收来自下层的消息并传给上层
        let (to_communicator_inbound_msg_tx, from_manager_inbound_msg_rx) =
            mpsc::channel::<HsmsMessage>(32);
        let (to_communicator_state_tx, from_manager_state_rx) =
            watch::channel::<ConnectionState>(ConnectionState::NotConnected);
        let system_bytes = Arc::new(SystemBytesGenerator::default());

        let manager = HsmsManager::new(
            config,
            from_communicator_cmd_rx,
            to_communicator_inbound_msg_tx,
            to_communicator_state_tx,
            Arc::clone(&system_bytes),
        );

        tokio::spawn(manager.run());

        let communicator = HsmsCommunicator {
            to_manager_cmd_tx,
            from_manager_state_rx,
            system_bytes,
        };

        (communicator, from_manager_inbound_msg_rx)
    }

    pub fn next_system_bytes(&self) -> u32 {
        self.system_bytes.next()
    }

    pub async fn send_reply(&self, msg: HsmsMessage) -> Result<(), HsmsError> {
        let command = HsmsCommand::SendReply { msg };

        // Send the command to the manager
        self.to_manager_cmd_tx
            .send(command)
            .await
            .map_err(|_| HsmsError::ChannelClosed { op: "send_reply" })?;

        Ok(())
    }

    pub async fn send_message(&self, msg: HsmsMessage) -> Result<(), HsmsError> {
        // cmd_tx 发送给 Manager 不需要回复
        let command = HsmsCommand::SendMessage { msg };

        self.to_manager_cmd_tx
            .send(command)
            .await
            .map_err(|_| HsmsError::ChannelClosed { op: "send_message" })
    }

    pub async fn send_message_with_reply(&self, msg: HsmsMessage) -> Result<HsmsMessage, HsmsError> {
        self.send_and_await_reply(
            |reply_tx| HsmsCommand::SendMessageNeedReply { msg, reply_tx },
            "send_message_with_reply",
        )
        .await
    }

    /// 获取当前连接状态
    ///
    /// 返回 HSMS 层的连接状态（NotConnected / NotSelected / Selected）。
    /// 如果已使用 `GemCommunicator`，建议优先使用 `GemCommunicator::state()` 获取含 GEM 子状态的完整设备状态。
    pub fn state(&self) -> ConnectionState {
        // 读取 state_rx 的当前值
        *self.from_manager_state_rx.borrow()
    }

    /// 获取连接状态监听通道
    ///
    /// 返回 HSMS 层的 3 种连接状态。适合需要自行实现 GEM 层的场景。
    /// 如果已使用 `GemCommunicator`，建议使用 `GemCommunicator::state_rx()` 获取完整的 `DeviceState`。
    pub fn state_rx(&self) -> watch::Receiver<ConnectionState> {
        self.from_manager_state_rx.clone()
    }

    // 获取 cmd_tx 用于业务层发送命令给 Manager
    pub fn cmd_tx(&self) -> mpsc::Sender<HsmsCommand> {
        self.to_manager_cmd_tx.clone()
    }

    pub async fn send_not_connect(&self) -> Result<(), HsmsError> {
        self.send_and_await_reply(
            |tx| HsmsCommand::NotConnect { reply_tx: tx },
            "send_not_connect",
        )
        .await
        .map(|_| ())
    }

    pub async fn send_not_select(&self) -> Result<(), HsmsError> {
        self.send_and_await_reply(
            |tx| HsmsCommand::NotSelect { reply_tx: tx },
            "send_not_select",
        )
        .await
        .map(|_| ())
    }

    pub async fn send_select(&self) -> Result<(), HsmsError> {
        self.send_and_await_reply(
            |tx| HsmsCommand::Select { reply_tx: tx },
            "send_select",
        )
        .await
        .map(|_| ())
    }

    pub async fn shutdown(&self) -> Result<(), HsmsError> {
        self.send_and_await_reply(
            |tx| HsmsCommand::Shutdown { reply_tx: tx },
            "shutdown",
        )
        .await
    }

    async fn send_and_await_reply<R: Send>(
        &self,
        make_command: impl FnOnce(oneshot::Sender<Result<R, HsmsError>>) -> HsmsCommand,
        op: &'static str,
    ) -> Result<R, HsmsError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let command = make_command(reply_tx);
        self.to_manager_cmd_tx
            .send(command)
            .await
            .map_err(|_| HsmsError::ChannelClosed { op })?;
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => Err(HsmsError::ReplyDropped {
                message: op.to_string(),
            }),
        }
    }
}
