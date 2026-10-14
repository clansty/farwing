// SPDX-License-Identifier: MIT
// Derived from oxideav-h265 0.0.10, Copyright (c) 2026 Karpelès Lab Inc.
// See crates/codec/kernels/hevc/COPYING.OxideAV.
//! §8.3.4 参考图像列表构造。
//!
//! DXVA 的短 slice 参数把这份活留给驱动，共用的 HEVC 语法 crate 因此只提供
//! [`openuuyc_codec::syntax::hevc::dpb::build_rps_poc_lists`]（§8.3.2 的 POC
//! 列表）；VA-API 要我们自己给出 RefPicList0/1，于是和 H.264 的
//! [`super::ref_list`] 一样放在平台层。

/// RefPicListTemp0/1 的循环不够长时补的内容。这里不支持
/// `pps_curr_pic_ref_enabled_flag`（那会让当前图像作为哨兵出现在列表里），
/// 所以没有任何一个真实下标用得上它。
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct RefPicListParams<'a> {
    /// `num_ref_idx_l0_active_minus1`
    pub num_ref_idx_l0_active_minus1: u32,
    /// `num_ref_idx_l1_active_minus1`
    pub num_ref_idx_l1_active_minus1: u32,
    /// `NumPicTotalCurr`（§7.4.7.2）
    pub num_pic_total_curr: u32,
    /// B 片才构造 RefPicList1
    pub is_b: bool,
    /// `list_entry_l0[]`（§7.3.6.2），有信号时用它重排 RefPicListTemp0
    pub list_entry_l0: Option<&'a [u32]>,
    /// `list_entry_l1[]`（§7.3.6.2），有信号时用它重排 RefPicListTemp1
    pub list_entry_l1: Option<&'a [u32]>,
}

/// §8.3.2 解析出来的 RPS，元素是 DPB 下标。`None` 对应规范里的「没有参考图像」。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ResolvedRps {
    /// `RefPicSetStCurrBefore`
    pub st_curr_before: Vec<Option<usize>>,
    /// `RefPicSetStCurrAfter`
    pub st_curr_after: Vec<Option<usize>>,
    /// `RefPicSetStFoll`
    pub st_foll: Vec<Option<usize>>,
    /// `RefPicSetLtCurr`
    pub lt_curr: Vec<Option<usize>>,
    /// `RefPicSetLtFoll`
    pub lt_foll: Vec<Option<usize>>,
}

/// §8.3.4 的参考图像列表，元素同样是 DPB 下标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RefPicLists {
    /// `RefPicList0`
    pub list0: Vec<Option<usize>>,
    /// `RefPicList1`，只有 B 片有
    pub list1: Option<Vec<Option<usize>>>,
}

/// §8.3.4：先按公式 8-8 / 8-10 循环取用凑出 RefPicListTempX，再按公式
/// 8-9 / 8-11 取前 `NumRefIdxActive` 个、必要时经 `list_entry_lX` 重排。
pub(super) fn build_ref_pic_lists(rps: &ResolvedRps, params: &RefPicListParams<'_>) -> RefPicLists {
    let active0 = (params.num_ref_idx_l0_active_minus1 + 1) as usize;
    let num_temp0 = active0.max(params.num_pic_total_curr as usize);
    let temp0 = build_temp_list(
        num_temp0,
        &[&rps.st_curr_before, &rps.st_curr_after, &rps.lt_curr],
    );
    let list0 = apply_list_modification(&temp0, active0, params.list_entry_l0);
    let list1 = if params.is_b {
        // 公式 8-10 把 Cur 的前后两段换了顺序。
        let active1 = (params.num_ref_idx_l1_active_minus1 + 1) as usize;
        let num_temp1 = active1.max(params.num_pic_total_curr as usize);
        let temp1 = build_temp_list(
            num_temp1,
            &[&rps.st_curr_after, &rps.st_curr_before, &rps.lt_curr],
        );
        Some(apply_list_modification(
            &temp1,
            active1,
            params.list_entry_l1,
        ))
    } else {
        None
    };
    RefPicLists { list0, list1 }
}

/// 公式 8-8 / 8-10：三段依次取用，取完一轮再从头来，直到凑够 `len` 个。
fn build_temp_list(len: usize, sets: &[&[Option<usize>]]) -> Vec<Option<usize>> {
    let mut temp = Vec::with_capacity(len);
    // 一个参考图像都没有时没有可循环的内容，再转下去就是死循环。
    if sets.iter().all(|set| set.is_empty()) {
        return temp;
    }
    while temp.len() < len {
        for set in sets {
            for &entry in *set {
                if temp.len() >= len {
                    break;
                }
                temp.push(entry);
            }
        }
    }
    temp
}

/// 公式 8-9 / 8-11：取前 `active` 个，`list_entry` 越界或指向空位就留空。
fn apply_list_modification(
    temp: &[Option<usize>],
    active: usize,
    list_entry: Option<&[u32]>,
) -> Vec<Option<usize>> {
    (0..active)
        .map(|r_idx| match list_entry {
            Some(entries) => entries
                .get(r_idx)
                .and_then(|index| temp.get(*index as usize).copied().flatten()),
            None => temp.get(r_idx).copied().flatten(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rps(before: &[usize], after: &[usize], lt: &[usize]) -> ResolvedRps {
        ResolvedRps {
            st_curr_before: before.iter().copied().map(Some).collect(),
            st_curr_after: after.iter().copied().map(Some).collect(),
            lt_curr: lt.iter().copied().map(Some).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn p_slice_list0_is_cur_before_then_after() {
        let params = RefPicListParams {
            num_ref_idx_l0_active_minus1: 2,
            num_pic_total_curr: 3,
            ..Default::default()
        };
        let lists = build_ref_pic_lists(&rps(&[1, 2], &[3], &[]), &params);
        assert_eq!(lists.list0, vec![Some(1), Some(2), Some(3)]);
        assert_eq!(lists.list1, None);
    }

    #[test]
    fn short_rps_cycles_until_the_active_count_is_filled() {
        // 公式 8-8：活动参考数比 NumPicTotalCurr 大时从头循环取用。
        let params = RefPicListParams {
            num_ref_idx_l0_active_minus1: 3,
            num_pic_total_curr: 2,
            ..Default::default()
        };
        let lists = build_ref_pic_lists(&rps(&[1], &[2], &[]), &params);
        assert_eq!(lists.list0, vec![Some(1), Some(2), Some(1), Some(2)]);
    }

    #[test]
    fn b_slice_list1_puts_the_following_pictures_first() {
        // 公式 8-10：RefPicListTemp1 是 Cur 之后、Cur 之前、长期，长期垫尾。
        let params = RefPicListParams {
            num_ref_idx_l0_active_minus1: 1,
            num_ref_idx_l1_active_minus1: 2,
            num_pic_total_curr: 3,
            is_b: true,
            ..Default::default()
        };
        let lists = build_ref_pic_lists(&rps(&[1], &[2], &[3]), &params);
        assert_eq!(lists.list0, vec![Some(1), Some(2)]);
        assert_eq!(lists.list1, Some(vec![Some(2), Some(1), Some(3)]));
    }

    #[test]
    fn list_entry_modification_reorders_ref_pic_list0() {
        let entries = [2u32, 0, 1];
        let params = RefPicListParams {
            num_ref_idx_l0_active_minus1: 2,
            num_pic_total_curr: 3,
            list_entry_l0: Some(&entries),
            ..Default::default()
        };
        let lists = build_ref_pic_lists(&rps(&[1], &[2], &[3]), &params);
        assert_eq!(lists.list0, vec![Some(3), Some(1), Some(2)]);
    }

    #[test]
    fn empty_rps_keeps_the_slots_empty() {
        let params = RefPicListParams {
            num_ref_idx_l0_active_minus1: 2,
            ..Default::default()
        };
        let lists = build_ref_pic_lists(&ResolvedRps::default(), &params);
        assert_eq!(lists.list0, vec![None, None, None]);
    }
}
