//! Cross-adapter frame delivery. Linux capture and encoding share the one CPU
//! path, so a transfer is never needed and cannot be created.
use super::capture::{Device, Frame};
use anyhow::{Result, bail};

pub(crate) struct Delivery {
    pub frame: Frame,
}

pub(crate) enum Transfer {}
impl Transfer {
    pub fn new(_source: &Device, _destination: &Device, _frame: &Frame) -> Result<Self> {
        bail!("Linux 采集与编码共用同一设备，不需要跨设备传输")
    }
    pub fn matches(&self, _source: &Device, _destination: &Device, _frame: &Frame) -> bool {
        match *self {}
    }
    /// `active` 是 Windows 那边读回时的取消闸（那边会轮询驱动忙不忙）；这个类型
    /// 一个值都构造不出来，参数只为和共享的调用处对上形状。
    pub fn copy(&mut self, _frame: &Frame, _active: impl Fn() -> bool) -> Result<Option<Delivery>> {
        match *self {}
    }
}
