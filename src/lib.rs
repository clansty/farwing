//! OpenUUYC: UU protocol interoperability and Windows application composition.

pub const APP_NAME: &str = "OpenUUYC";
pub(crate) const VIEWER_TITLE_PREFIX: &str = "OpenUUYC — ";

pub mod account;
pub mod application;
pub mod diagnostics;
pub mod features;
mod lifecycle;
pub mod media;
pub mod protocol;
pub mod session;
pub mod transport;

pub mod plugins;
mod ui;

/// 本机自测用：同一进程里既开观看又开终端，验证两者共用一条房间 socket。
pub async fn debug_dual_probe(
    alias: &str,
    terminal_first: bool,
    with_files: bool,
) -> anyhow::Result<()> {
    let client = std::sync::Arc::new(account::client::AuthenticatedClient::from_saved_session()?);
    let list = client.list_devices().await?;
    let device = list
        .my_binded_devices
        .iter()
        .find(|device| device.alias == alias)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("账号里没有这台设备"))?;
    if terminal_first {
        let handle =
            features::terminal::service::start_for(client.clone(), &device.device_id).await?;
        println!("终端先连：{} 条会话", handle.list().await?.len());
        let options = media::preferences::load()?;
        let (connection, _summary) =
            session::controller::connect_saved_alias(alias, options).await?;
        println!("观看后连：已建立");
        drop(connection);
        println!("观看已关闭");
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        let sessions = handle.list().await?;
        println!("关掉观看后终端仍有 {} 条会话", sessions.len());
        features::terminal::service::shutdown_all().await;
        client.close().await;
        return Ok(());
    }
    let options = media::preferences::load()?;
    let (connection, _summary) = session::controller::connect_saved_alias(alias, options).await?;
    println!("观看会话已建立");
    let mut started = false;
    for _ in 0..15 {
        if connection.activate_viewing().await.is_ok() {
            started = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    println!("观看已激活：{started}");
    let handle = features::terminal::service::start_for(client.clone(), &device.device_id).await?;
    let sessions = handle.list().await?;
    println!("终端会话 {} 条", sessions.len());
    let mut attach = handle.attach(None, Some(format!("probe-{}", std::process::id())), 80, 24)?;
    let info = attach
        .ready
        .await
        .map_err(|_| anyhow::anyhow!("终端服务未回复"))?
        .map_err(anyhow::Error::msg)?;
    println!("已接上新会话 {} （{}）", info.handle, info.name);
    attach
        .input
        .send(features::terminal::service::ClientInput::Data(
            b"echo probe-ok\r\n".to_vec(),
        ))
        .await
        .ok();
    let mut collected = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(deadline, attach.output.recv()).await {
            Ok(Some(features::terminal::service::ServiceOutput::Data(bytes))) => {
                collected.extend_from_slice(&bytes)
            }
            _ => break,
        }
    }
    println!(
        "终端输出 {} 字节：\n{}",
        collected.len(),
        String::from_utf8_lossy(&collected)
    );
    println!("终端连上时观看仍在：{}", connection.alive());
    let closed = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        connection.close(),
    )
    .await;
    println!("结束观看按时返回：{}", closed.is_ok());
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let sessions = handle.list().await?;
    println!("关掉观看后终端仍有 {} 条会话", sessions.len());
    if with_files {
        let files = features::file_transfer::service::start(
            client.clone(),
            device.clone(),
            media::preferences::load()?,
        );
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        println!(
            "文件传输后界面认为有观看会话：{}",
            session::controller::has_gui_connection(&client.device_id(), &device.device_id)
        );
        println!("文件传输期间终端仍有 {} 条会话", handle.list().await?.len());
        drop(files);
    }
    features::terminal::service::shutdown_all().await;
    client.close().await;
    Ok(())
}

mod platform;
