//! 音频线程的进程级初始化。
//!
//! Windows 那边要把输出线程初始化成 COM MTA，否则 WASAPI 在错误的公寓里跑；
//! 其他平台没有这回事。留着一模一样的接口，是为了让 `media::audio` 那段共用代码
//! 不必到处开 `cfg`——两边都问一句「初始化了吗」，答案不一样而已。
use anyhow::Result;

/// 不需要初始化的平台上，这一步永远是成功的。
pub(crate) fn initialize() -> Result<()> {
    Ok(())
}

/// 上面那一步的结果，日志里跟着一起打。Windows 报的是公寓类型。
pub(crate) fn description() -> String {
    "没有 COM 公寓（非 Windows 平台）".to_owned()
}
