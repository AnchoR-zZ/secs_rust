//! GEM Communicator — GEM 层公共 API
//!
//! 上层应用使用的入口，内部封装 HsmsCommunicator + GemManager。
//! 与 HsmsCommunicator 并存，应用可自由选择使用哪个层级。

use crate::gem::config::GemConfig;
use crate::gem::DeviceState;
use crate::gem::manager::GemManager;
use crate::gem::{GemCommand, GemError};
use crate::hsms::communicator::HsmsCommunicator;
use crate::hsms::message::HsmsMessage;
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Clone)]
pub struct GemCommunicator {
    /// 发送命令给 GemManager
    to_manager_cmd_tx: mpsc::Sender<GemCommand>,
    /// 监控设备状态（DeviceState 含 HSMS + GEM 完整层级）
    from_manager_state_rx: watch::Receiver<DeviceState>,
}

impl GemCommunicator {
    /// 创建 GemCommunicator
    ///
    /// 返回 `(GemCommunicator, mpsc::Receiver<HsmsMessage>)`:
    /// - `GemCommunicator`: 用于发送命令和监控状态
    /// - `Receiver<HsmsMessage>`: 接收非 GEM 的透传消息
    pub fn new(config: GemConfig) -> (Self, mpsc::Receiver<HsmsMessage>) {
        // 1. 创建底层 HSMS 通信器
        let (hsms_communicator, hsms_inbound_rx) =
            HsmsCommunicator::new(config.hsms_config.clone());

        // 2. GemCommand 命令通道
        let (to_manager_cmd_tx, from_communicator_cmd_rx) = mpsc::channel::<GemCommand>(32);

        // 3. 透传消息通道（非 GEM 的消息转发给上层）
        let (to_communicator_inbound_msg_tx, from_manager_inbound_msg_rx) =
            mpsc::channel::<HsmsMessage>(32);

        // 4. GEM 状态广播通道
        let (to_communicator_state_tx, from_manager_state_rx) =
            watch::channel::<DeviceState>(DeviceState::NotConnected);

        // 5. 构造 GemManager
        let manager = GemManager::new(
            config,
            hsms_communicator,
            hsms_inbound_rx,
            from_communicator_cmd_rx,
            to_communicator_inbound_msg_tx,
            to_communicator_state_tx,
        );

        // 6. 启动 Manager
        tokio::spawn(manager.run());

        let communicator = GemCommunicator {
            to_manager_cmd_tx,
            from_manager_state_rx,
        };

        (communicator, from_manager_inbound_msg_rx)
    }

    // ========================================================================
    // GEM 控制操作
    // ========================================================================

    /// 操作员请求上线（触发状态机 #3: EquipmentOffLine → AttemptOnLine）
    pub async fn operator_online(&self) -> Result<(), GemError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.to_manager_cmd_tx
            .send(GemCommand::OperatorOnline { reply_tx })
            .await
            .map_err(|_| GemError::ChannelClosed { op: "operator_online" })?;
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => Err(GemError::ReplyDropped {
                message: "operator_online".into(),
            }),
        }
    }

    /// 操作员请求下线（触发状态机 #6/#12: OnLine/HostOffLine → EquipmentOffLine）
    pub async fn operator_offline(&self) -> Result<(), GemError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.to_manager_cmd_tx
            .send(GemCommand::OperatorOffline { reply_tx })
            .await
            .map_err(|_| GemError::ChannelClosed { op: "operator_offline" })?;
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => Err(GemError::ReplyDropped {
                message: "operator_offline".into(),
            }),
        }
    }

    /// 切换为 Local 模式（触发状态机 #9: Remote → Local）
    pub async fn set_local(&self) -> Result<(), GemError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.to_manager_cmd_tx
            .send(GemCommand::SetLocal { reply_tx })
            .await
            .map_err(|_| GemError::ChannelClosed { op: "set_local" })?;
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => Err(GemError::ReplyDropped {
                message: "set_local".into(),
            }),
        }
    }

    /// 切换为 Remote 模式（触发状态机 #8: Local → Remote）
    pub async fn set_remote(&self) -> Result<(), GemError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.to_manager_cmd_tx
            .send(GemCommand::SetRemote { reply_tx })
            .await
            .map_err(|_| GemError::ChannelClosed { op: "set_remote" })?;
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => Err(GemError::ReplyDropped {
                message: "set_remote".into(),
            }),
        }
    }

    // ========================================================================
    // SECS-II 消息透传
    // ========================================================================

    /// 发送 SECS-II 数据消息（fire-and-forget，不等回复）
    pub async fn send_message(&self, msg: HsmsMessage) -> Result<(), GemError> {
        self.to_manager_cmd_tx
            .send(GemCommand::SendMessage { msg })
            .await
            .map_err(|_| GemError::ChannelClosed { op: "send_message" })
    }

    /// 发送 SECS-II 数据消息并等待回复
    pub async fn send_message_with_reply(
        &self,
        msg: HsmsMessage,
    ) -> Result<HsmsMessage, GemError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.to_manager_cmd_tx
            .send(GemCommand::SendMessageWithReply { msg, reply_tx })
            .await
            .map_err(|_| GemError::ChannelClosed { op: "send_message_with_reply" })?;
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => Err(GemError::ReplyDropped {
                message: "send_message_with_reply".into(),
            }),
        }
    }

    /// 发送回复消息
    pub async fn send_reply(&self, msg: HsmsMessage) -> Result<(), GemError> {
        self.to_manager_cmd_tx
            .send(GemCommand::SendReply { msg })
            .await
            .map_err(|_| GemError::ChannelClosed { op: "send_reply" })
    }

    // ========================================================================
    // 状态查询
    // ========================================================================

    /// 获取当前设备状态（完整层级，含 HSMS 连接状态 + GEM 控制子状态）
    pub fn state(&self) -> DeviceState {
        self.from_manager_state_rx.borrow().clone()
    }

    /// 获取状态监听通道（`DeviceState` 含完整状态层级）
    pub fn state_rx(&self) -> watch::Receiver<DeviceState> {
        self.from_manager_state_rx.clone()
    }

    // ========================================================================
    // 生命周期
    // ========================================================================

    /// 关闭 GEM 层（同时关闭底层 HSMS）
    pub async fn shutdown(&self) -> Result<(), GemError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.to_manager_cmd_tx
            .send(GemCommand::Shutdown { reply_tx })
            .await
            .map_err(|_| GemError::ChannelClosed { op: "shutdown" })?;
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => Err(GemError::ReplyDropped {
                message: "shutdown".into(),
            }),
        }
    }
}
