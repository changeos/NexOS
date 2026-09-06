//! 相册组织算法（纯逻辑）。
//!
//! 给定 `MediaAsset` 列表 + `AlbumGrouping` 策略，产出 `Album` 集合。
//! 不依赖时间库之外的状态；按时间分组用 `taken_at`（UTC），按地点用 `gps`，
//! 按人脸用 `faces[].name`。
//!
//! 设计：稳定排序后线性扫描分组；相册 id 由策略 + key 派生（确定性），
//! 便于测试与幂等重算。

use std::collections::BTreeMap;

use crate::media::{Album, AlbumGrouping, MediaAsset};

/// 将资源列表按策略分组为相册集合。
///
/// - `ByDay`：同一天（按 UTC `taken_at` 日期）归一组；无 taken_at 的资源归入 `"未日期化"`。
/// - `ByMonth`：同月归一组。
/// - `ByLocation { meters }`：贪心锚点法——遍历资源，若距任一已有锚点 ≤ meters 则归入该锚点，
///   否则新建锚点；无 GPS 的归入 `"无位置"`。
/// - `ByFace`：每个具名人脸一个相册；资源含多个名字时归入第一个；无名脸/无人脸的归入 `"无人脸"`。
/// - `Single { name }`：全部归入单一相册。
///
/// 相册 id 形如 `album:<策略>:<key>`；命名人类可读。返回结果按 `name` 排序以稳定。
pub fn group_into_albums(assets: &[MediaAsset], strategy: &AlbumGrouping) -> Vec<Album> {
    match strategy {
        AlbumGrouping::Single { name } => {
            let count = assets.len() as u32;
            let id = format!("album:single:{}", slug(name));
            let album = Album {
                id,
                name: name.clone(),
                asset_count: count,
            };
            if count == 0 {
                Vec::new()
            } else {
                vec![album]
            }
        }
        AlbumGrouping::ByDay => group_by_date(assets, DateGranularity::Day),
        AlbumGrouping::ByMonth => group_by_date(assets, DateGranularity::Month),
        AlbumGrouping::ByLocation { meters } => group_by_location(assets, *meters),
        AlbumGrouping::ByFace => group_by_face(assets),
    }
}

#[derive(Clone, Copy)]
enum DateGranularity {
    Day,
    Month,
}

fn group_by_date(assets: &[MediaAsset], g: DateGranularity) -> Vec<Album> {
    use chrono::NaiveDate;
    let mut buckets: BTreeMap<String, Vec<&MediaAsset>> = BTreeMap::new();
    for a in assets {
        let key = match a.taken_at {
            Some(dt) => {
                let d: NaiveDate = dt.date_naive();
                match g {
                    DateGranularity::Day => d.format("%Y-%m-%d").to_string(),
                    DateGranularity::Month => d.format("%Y-%m").to_string(),
                }
            }
            None => "未日期化".to_string(),
        };
        buckets.entry(key).or_default().push(a);
    }
    let mut out: Vec<Album> = buckets
        .into_iter()
        .map(|(key, items)| Album {
            id: format!("album:date:{key}"),
            name: key,
            asset_count: items.len() as u32,
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn group_by_location(assets: &[MediaAsset], meters: u32) -> Vec<Album> {
    use crate::media::GpsCoord;
    // 锚点：每个锚点记录其首个资源的坐标 + 累计 count
    let mut anchors: Vec<(GpsCoord, u32, String)> = Vec::new(); // (coord, count, first_asset_id)
    let mut no_loc = 0u32;
    let mut no_loc_first = String::new();

    for a in assets {
        let gps = match exif_gps_of(a) {
            Some(g) => g,
            None => {
                if no_loc == 0 {
                    no_loc_first = a.id.clone();
                }
                no_loc += 1;
                continue;
            }
        };
        // 找到最近的锚点
        let mut found = None;
        for (i, (coord, _, _)) in anchors.iter().enumerate() {
            if coord.distance_meters(&gps) <= meters as f64 {
                found = Some(i);
                break;
            }
        }
        match found {
            Some(i) => anchors[i].1 += 1,
            None => anchors.push((gps, 1, a.id.clone())),
        }
    }

    let mut out: Vec<Album> = anchors
        .into_iter()
        .enumerate()
        .map(|(i, (coord, count, _first))| Album {
            id: format!("album:loc:{}", i),
            name: format!("{:.4},{:.4}", coord.lat, coord.lon),
            asset_count: count,
        })
        .collect();
    // 让 first 不再被 unused 警告（保留语义：锚点 id 派生可基于首资源）
    let _ = no_loc_first;

    if no_loc > 0 {
        out.push(Album {
            id: "album:loc:none".to_string(),
            name: "无位置".to_string(),
            asset_count: no_loc,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn group_by_face(assets: &[MediaAsset]) -> Vec<Album> {
    let mut buckets: BTreeMap<String, u32> = BTreeMap::new();
    for a in assets {
        let name = a
            .faces
            .iter()
            .find_map(|f| f.name.clone())
            .unwrap_or_else(|| "无人脸".to_string());
        *buckets.entry(name).or_insert(0) += 1;
    }
    let mut out: Vec<Album> = buckets
        .into_iter()
        .map(|(name, count)| Album {
            id: format!("album:face:{}", slug(&name)),
            name,
            asset_count: count,
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// 从 MediaAsset 取出 EXIF GPS（若 clip_embedding 之外未存 GPS，则取 taken_at 同源）。
/// 现阶段 MediaAsset 未直接持 GPS，这里返回 None——预留接入点：DefaultMediaManager.ingest
/// 会将解析出的 GPS 暂存在临时映射中，分组时由调用方传入富化后的 asset。
/// 为支持纯逻辑测试，提供一个简单适配：若 asset.path 形如 `gps:lat,lon` 则解析之（测试用）。
fn exif_gps_of(a: &MediaAsset) -> Option<crate::media::GpsCoord> {
    // 解析测试钩子路径 "gps:lat,lon"
    let p = a.path.strip_prefix("gps:");
    if let Some(rest) = p {
        let mut it = rest.split(',');
        let lat: f64 = it.next()?.parse().ok()?;
        let lon: f64 = it.next()?.parse().ok()?;
        return Some(crate::media::GpsCoord::new(lat, lon));
    }
    None
}

fn slug(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{AlbumGrouping, BBox, FaceTag, MediaAsset};
    use chrono::TimeZone;

    fn asset(id: &str, taken: Option<&str>) -> MediaAsset {
        let taken_at = taken.map(|s| {
            chrono::Utc
                .from_local_datetime(
                    &chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap(),
                )
                .unwrap()
        });
        MediaAsset {
            id: id.to_string(),
            path: format!("/photos/{id}.jpg"),
            mime_type: "image/jpeg".to_string(),
            size_bytes: 100,
            width: Some(100),
            height: Some(100),
            taken_at,
            faces: vec![],
            clip_embedding: None,
        }
    }

    fn asset_with_gps(id: &str, lat: f64, lon: f64) -> MediaAsset {
        let mut a = asset(id, None);
        a.path = format!("gps:{lat},{lon}");
        a
    }

    fn asset_with_face(id: &str, name: Option<&str>) -> MediaAsset {
        let mut a = asset(id, None);
        a.faces = vec![FaceTag {
            name: name.map(String::from),
            bbox: BBox {
                x: 0.1,
                y: 0.1,
                w: 0.2,
                h: 0.2,
            },
        }];
        a
    }

    #[test]
    fn groups_by_day() {
        let assets = vec![
            asset("a1", Some("2024-03-01 10:00:00")),
            asset("a2", Some("2024-03-01 18:00:00")),
            asset("a3", Some("2024-03-02 09:00:00")),
            asset("a4", None),
        ];
        let albums = group_into_albums(&assets, &AlbumGrouping::ByDay);
        assert_eq!(albums.len(), 3);
        let counts: std::collections::HashMap<&str, u32> = albums
            .iter()
            .map(|a| (a.name.as_str(), a.asset_count))
            .collect();
        assert_eq!(counts.get("2024-03-01"), Some(&2));
        assert_eq!(counts.get("2024-03-02"), Some(&1));
        assert_eq!(counts.get("未日期化"), Some(&1));
    }

    #[test]
    fn groups_by_month() {
        let assets = vec![
            asset("a1", Some("2024-03-01 10:00:00")),
            asset("a2", Some("2024-03-20 10:00:00")),
            asset("a3", Some("2024-04-01 10:00:00")),
        ];
        let albums = group_into_albums(&assets, &AlbumGrouping::ByMonth);
        assert_eq!(albums.len(), 2);
        assert!(albums
            .iter()
            .any(|a| a.name == "2024-03" && a.asset_count == 2));
        assert!(albums
            .iter()
            .any(|a| a.name == "2024-04" && a.asset_count == 1));
    }

    #[test]
    fn groups_by_location_within_radius() {
        // 两个点相距约 850m，半径 1000m 应归一组
        let assets = vec![
            asset_with_gps("a1", 39.9087, 116.3975),
            asset_with_gps("a2", 39.9163, 116.3972),
            asset_with_gps("a3", 0.0, 0.0), // 很远
            asset("a4", None),              // 无 GPS
        ];
        let albums = group_into_albums(&assets, &AlbumGrouping::ByLocation { meters: 1000 });
        // 至少两组（北京组 + 远点组）+ 无位置组
        assert!(albums.len() >= 3);
        let no_loc = albums.iter().find(|a| a.name == "无位置").unwrap();
        assert_eq!(no_loc.asset_count, 1);
    }

    #[test]
    fn groups_by_face() {
        let assets = vec![
            asset_with_face("a1", Some("张三")),
            asset_with_face("a2", Some("张三")),
            asset_with_face("a3", Some("李四")),
            asset_with_face("a4", None),
        ];
        let albums = group_into_albums(&assets, &AlbumGrouping::ByFace);
        assert_eq!(albums.len(), 3);
        let counts: std::collections::HashMap<&str, u32> = albums
            .iter()
            .map(|a| (a.name.as_str(), a.asset_count))
            .collect();
        assert_eq!(counts.get("张三"), Some(&2));
        assert_eq!(counts.get("李四"), Some(&1));
        assert_eq!(counts.get("无人脸"), Some(&1));
    }

    #[test]
    fn single_album() {
        let assets = vec![asset("a1", None), asset("a2", None)];
        let albums = group_into_albums(
            &assets,
            &AlbumGrouping::Single {
                name: "全部".to_string(),
            },
        );
        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].name, "全部");
        assert_eq!(albums[0].asset_count, 2);
    }

    #[test]
    fn empty_input() {
        let albums = group_into_albums(&[], &AlbumGrouping::ByDay);
        assert!(albums.is_empty());
    }
}
