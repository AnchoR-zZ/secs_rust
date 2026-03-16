use crate::hsms::manager::HsmsManager;
use crate::hsms::message::HsmsMessage;
use crate::hsms::{ConnectionState, HsmsCommand, HsmsError, config::HsmsConfig};
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Clone)]
pub struct HsmsCommunicator {
    // 发送命令给管理器的通道
    to_manager_cmd_tx: mpsc::Sender<HsmsCommand>,
    // 实时监控连接状态
    from_manager_state_rx: watch::Receiver<ConnectionState>,
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

        let manager = HsmsManager::new(
            config,
            from_communicator_cmd_rx,
            to_communicator_inbound_msg_tx,
            to_communicator_state_tx,
        );

        tokio::spawn(manager.run());

        let communicator = HsmsCommunicator {
            to_manager_cmd_tx,
            from_manager_state_rx,
        };

        (communicator, from_manager_inbound_msg_rx)
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
        // cmd_tx 发送给 Manager 如果需要回复则等待回复后返回
        let (reply_tx, reply_rx) = oneshot::channel();

        let command = HsmsCommand::SendMessageNeedReply { msg, reply_tx };

        // Send the command to the manager
        self.to_manager_cmd_tx
            .send(command)
            .await
            .map_err(|_| HsmsError::ChannelClosed { op: "send_message_with_reply" })?;

        // Wait for the reply
        match reply_rx.await {
            Ok(Ok(response)) => Ok(response), // Response received successfully
            Ok(Err(e)) => Err(e),
            Err(_) => Err(HsmsError::ReplyDropped {
                message: "send_message_with_reply".to_string(),
            }),
        }
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
        let (reply_tx, reply_rx) = oneshot::channel();
        let command = HsmsCommand::NotConnect { reply_tx };
        self.to_manager_cmd_tx
            .send(command)
            .await
            .map_err(|_| HsmsError::ChannelClosed { op: "send_not_connect" })?;
        match reply_rx.await {
            Ok(Ok(_)) => Ok(()), // Response received successfully
            Ok(Err(e)) => Err(e),
            Err(_) => Err(HsmsError::ReplyDropped {
                message: "send_not_connect".to_string(),
            }),
        }
    }

    pub async fn send_not_select(&self) -> Result<(), HsmsError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let command = HsmsCommand::NotSelect { reply_tx };
        self.to_manager_cmd_tx
            .send(command)
            .await
            .map_err(|_| HsmsError::ChannelClosed { op: "send_not_select" })?;
        match reply_rx.await {
            Ok(Ok(_)) => Ok(()), // Response received successfully
            Ok(Err(e)) => Err(e),
            Err(_) => Err(HsmsError::ReplyDropped {
                message: "send_not_select".to_string(),
            }),
        }
    }

    pub async fn send_select(&self) -> Result<(), HsmsError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let command = HsmsCommand::Select { reply_tx };
        self.to_manager_cmd_tx
            .send(command)
            .await
            .map_err(|_| HsmsError::ChannelClosed { op: "send_select" })?;
        match reply_rx.await {
            Ok(Ok(_)) => Ok(()), // Response received successfully
            Ok(Err(e)) => Err(e),
            Err(_) => Err(HsmsError::ReplyDropped {
                message: "send_select".to_string(),
            }),
        }
    }

    /// 发送 Shutdown 命令，停止 manager
    pub async fn shutdown(&self) -> Result<(), HsmsError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let command = HsmsCommand::Shutdown { reply_tx };
        self.to_manager_cmd_tx
            .send(command)
            .await
            .map_err(|_| HsmsError::ChannelClosed { op: "shutdown" })?;
        match reply_rx.await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(HsmsError::ReplyDropped {
                message: "shutdown".to_string(),
            }),
        }
    }
}
