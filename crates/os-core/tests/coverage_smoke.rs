//! os-core 覆盖率补测——纯 DTO/newtype 冒烟测。
//!
//! 目标：把 `types.rs`/`ids.rs`（原 0% 覆盖）拉到全覆盖，并补 `error.rs` 的
//! `Display` 实现（thiserror `#[error]` 派生的 fmt 体默认无单测调用，是各 crate
//! `error.rs` 全员 0% 的通因——本文件对每个变体断言 `format!("{e}")`）。
//!
//! 每个类型断言四件事（适用项）：
//! 1. 构造（new/Default/字段直接构造）
//! 2. Debug/Display 格式化（证明派生实现可用且不 panic）
//! 3. serde 序列化往返（证明 Serialize/Deserialize 派生对称）
//! 4. PartialEq 比较 / AsRef/From 转换
//!
//! 默认全部跑（无 `#[ignore]`），因为这些都是纯内存冒烟测，零外部依赖。

use os_core::{
    AddressId, ChainId, CommandOutput, ContainerId, CoreError, DatasetId, GuestId, Health,
    HealthReport, NodeId, NodeInfo, NodeRole, PageRequest, PageResponse, PoolId, ResourceQuota,
    ShareId, SnapshotId, TaskId, VmId, VolumeId, WalletSessionId,
};
use serde_json::json;

// ============================================================================
// types.rs
// ============================================================================

mod types_tests {
    use super::*;

    #[test]
    fn health_serde_roundtrip_all_variants() {
        // rename_all = "snake_case" -> healthy/degraded/unhealthy/unknown
        let cases = [
            (Health::Healthy, "healthy"),
            (Health::Degraded, "degraded"),
            (Health::Unhealthy, "unhealthy"),
            (Health::Unknown, "unknown"),
        ];
        for (v, expect) in cases {
            let s = serde_json::to_string(&v).unwrap();
            assert_eq!(s, format!("\"{expect}\""));
            let back: Health = serde_json::from_str(&s).unwrap();
            assert_eq!(back, v, "serde 往返不对称：{expect}");
            // Debug 派生可用且不 panic
            let _ = format!("{v:?}");
        }
    }

    #[test]
    fn health_partial_eq_and_copy() {
        // Copy 派生：移动不应消耗
        let a = Health::Healthy;
        let b = a; // copy
        let _ = a; // 仍可用 -> 证 Copy
        assert_eq!(a, b);
        assert_ne!(a, Health::Unhealthy);
    }

    #[test]
    fn health_report_serde_and_debug() {
        let ts = chrono::Utc::now();
        let report = HealthReport {
            health: Health::Degraded,
            message: Some("RPC timeout".to_string()),
            timestamp: ts,
        };
        let json_val = serde_json::to_value(&report).unwrap();
        assert_eq!(json_val["health"], "degraded");
        assert_eq!(json_val["message"], "RPC timeout");

        let back: HealthReport = serde_json::from_value(json_val).unwrap();
        assert_eq!(back.health, Health::Degraded);
        assert_eq!(back.message.as_deref(), Some("RPC timeout"));
        assert_eq!(back.timestamp, ts);
        let _ = format!("{report:?}");
    }

    #[test]
    fn health_report_message_none() {
        let report = HealthReport {
            health: Health::Unknown,
            message: None,
            timestamp: chrono::Utc::now(),
        };
        let s = serde_json::to_string(&report).unwrap();
        assert!(s.contains("\"message\":null"));
        let back: HealthReport = serde_json::from_str(&s).unwrap();
        assert!(back.message.is_none());
    }

    #[test]
    fn capacity_free_bytes_and_ratio() {
        let c = Capacity {
            used_bytes: 30,
            total_bytes: 100,
        };
        assert_eq!(c.free_bytes(), 70);
        assert!((c.used_ratio() - 0.3).abs() < 1e-9);

        // used > total：saturating_sub 截到 0
        let over = Capacity {
            used_bytes: 150,
            total_bytes: 100,
        };
        assert_eq!(over.free_bytes(), 0);

        // total = 0：ratio 返回 0.0（防除零）
        let zero = Capacity {
            used_bytes: 5,
            total_bytes: 0,
        };
        assert_eq!(zero.free_bytes(), 0);
        assert_eq!(zero.used_ratio(), 0.0);

        // Debug/serde
        let json_val = serde_json::to_value(c).unwrap();
        assert_eq!(json_val["used_bytes"], 30);
        let _ = format!("{c:?}");
    }

    #[test]
    fn resource_quota_serde_none_and_some() {
        let full = ResourceQuota {
            cpu_cores: Some(0.5),
            memory_bytes: Some(1024),
            io_bps_limit: Some(2048),
        };
        let json_val = serde_json::to_value(&full).unwrap();
        assert_eq!(json_val["cpu_cores"], 0.5);
        assert_eq!(json_val["memory_bytes"], 1024);
        assert_eq!(json_val["io_bps_limit"], 2048);
        let back: ResourceQuota = serde_json::from_value(json_val).unwrap();
        assert_eq!(back.cpu_cores, Some(0.5));

        let unlimited = ResourceQuota {
            cpu_cores: None,
            memory_bytes: None,
            io_bps_limit: None,
        };
        let s = serde_json::to_string(&unlimited).unwrap();
        assert!(s.contains("\"cpu_cores\":null"));
        let _ = format!("{unlimited:?}");
    }

    #[test]
    fn node_role_serde_all_variants() {
        let cases = [
            (NodeRole::Leader, "leader"),
            (NodeRole::Follower, "follower"),
            (NodeRole::Peer, "peer"),
            (NodeRole::Standalone, "standalone"),
        ];
        for (v, expect) in cases {
            let s = serde_json::to_string(&v).unwrap();
            assert_eq!(s, format!("\"{expect}\""));
            let back: NodeRole = serde_json::from_str(&s).unwrap();
            assert_eq!(back, v);
            let _ = format!("{v:?}");
        }
    }

    #[test]
    fn node_info_serde_and_debug() {
        let ni = NodeInfo {
            node_id: NodeId::new("n1"),
            role: NodeRole::Leader,
            version: "0.1.0".to_string(),
            arch: "x86_64".to_string(),
            endpoints: vec!["10.0.0.1:8080".to_string()],
            health: Health::Healthy,
        };
        let json_val = serde_json::to_value(&ni).unwrap();
        assert_eq!(json_val["node_id"], "n1");
        assert_eq!(json_val["role"], "leader");
        assert_eq!(json_val["endpoints"][0], "10.0.0.1:8080");
        assert_eq!(json_val["health"], "healthy");

        let back: NodeInfo = serde_json::from_value(json_val).unwrap();
        assert_eq!(back.node_id.as_str(), "n1");
        assert_eq!(back.role, NodeRole::Leader);
        assert_eq!(back.version, "0.1.0");
        let _ = format!("{ni:?}");
    }

    #[test]
    fn page_request_default() {
        let d = PageRequest::default();
        assert_eq!(d.offset, 0);
        assert_eq!(d.limit, 50);

        let custom = PageRequest {
            offset: 100,
            limit: 25,
        };
        let json_val = serde_json::to_value(&custom).unwrap();
        assert_eq!(json_val["offset"], 100);
        assert_eq!(json_val["limit"], 25);
        let back: PageRequest = serde_json::from_value(json_val).unwrap();
        assert_eq!(back.offset, 100);
        let _ = format!("{custom:?}");
    }

    #[test]
    fn page_response_serde_generic() {
        let resp = PageResponse {
            items: vec!["a".to_string(), "b".to_string()],
            total: 2,
            offset: 0,
            limit: 10,
        };
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains("\"items\":[\"a\",\"b\"]"));
        let back: PageResponse<String> = serde_json::from_str(&s).unwrap();
        assert_eq!(back.items, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(back.total, 2);
        let _ = format!("{resp:?}");

        // 空列表边界
        let empty = PageResponse::<i32> {
            items: vec![],
            total: 0,
            offset: 0,
            limit: 10,
        };
        let _ = serde_json::to_string(&empty).unwrap();
    }

    #[test]
    fn command_output_constructors_and_success() {
        let ok = CommandOutput::ok();
        assert!(ok.is_success());
        assert!(ok.stdout.is_empty());
        assert!(ok.stderr.is_empty());
        assert_eq!(ok.exit_code, 0);

        let ok2 = CommandOutput::ok_with_stdout("hello");
        assert!(ok2.is_success());
        assert_eq!(ok2.stdout, "hello");
        assert!(ok2.stderr.is_empty());

        let fail = CommandOutput::fail(2, "boom");
        assert!(!fail.is_success());
        assert_eq!(fail.exit_code, 2);
        assert_eq!(fail.stderr, "boom");
        assert!(fail.stdout.is_empty());
    }

    #[test]
    fn command_output_default_eq_ok() {
        let d = CommandOutput::default();
        assert_eq!(d, CommandOutput::ok());
    }

    #[test]
    fn command_output_serde_and_eq() {
        let co = CommandOutput {
            stdout: "out".to_string(),
            stderr: "err".to_string(),
            exit_code: 7,
        };
        let json_val = serde_json::to_value(&co).unwrap();
        assert_eq!(json_val["stdout"], "out");
        assert_eq!(json_val["stderr"], "err");
        assert_eq!(json_val["exit_code"], 7);

        let back: CommandOutput = serde_json::from_value(json_val).unwrap();
        assert_eq!(back, co); // PartialEq
        let _ = format!("{co:?}");
    }
}

// ============================================================================
// ids.rs
// ============================================================================

mod ids_tests {
    use super::*;

    /// 对一个 string newtype 跑全套冒烟断言（构造/Display/as_str/From/serde）。
    /// 用 macro（而非泛型闭包）避免 `impl Into<String>` 构造器单态化导致的 HRTB 寿命问题。
    macro_rules! string_id_smoke {
        ($ty:ty, $val:expr) => {{
            let id = <$ty>::new($val);
            // as_str
            assert_eq!(id.as_str(), $val);
            // Display 等于内部 String
            assert_eq!(format!("{id}"), $val);
            // Debug 可用且不 panic
            let _ = format!("{id:?}");
            // serde 往返：序列化为 JSON 字符串
            let s = serde_json::to_string(&id).unwrap();
            assert_eq!(s, concat!("\"", $val, "\""));
            let back: $ty = serde_json::from_str(&s).unwrap();
            assert_eq!(back, id); // PartialEq
                                  // From<String> 转换
            let from_str: $ty = $val.to_string().into();
            assert_eq!(from_str, id);
            // Clone
            let cloned = id.clone();
            assert_eq!(cloned, id);
            // PartialEq ne
            assert_ne!(id, <$ty>::new("other-different"));
        }};
    }

    #[test]
    fn pool_id_smoke() {
        string_id_smoke!(PoolId, "tank");
    }

    #[test]
    fn dataset_id_smoke() {
        string_id_smoke!(DatasetId, "tank/media");
    }

    #[test]
    fn snapshot_id_smoke() {
        string_id_smoke!(SnapshotId, "tank/media@snap1");
    }

    #[test]
    fn vm_id_smoke() {
        string_id_smoke!(VmId, "vm-1");
    }

    #[test]
    fn container_id_smoke() {
        string_id_smoke!(ContainerId, "ct-1");
    }

    #[test]
    fn guest_id_smoke() {
        string_id_smoke!(GuestId, "GUEST-ABCDEF");
    }

    #[test]
    fn node_id_smoke() {
        string_id_smoke!(NodeId, "node-1");
    }

    #[test]
    fn share_id_smoke() {
        string_id_smoke!(ShareId, "share-1");
    }

    #[test]
    fn volume_id_smoke() {
        string_id_smoke!(VolumeId, "vol-1");
    }

    #[test]
    fn wallet_session_id_smoke() {
        string_id_smoke!(WalletSessionId, "wc-session-1");
    }

    #[test]
    fn chain_id_smoke() {
        string_id_smoke!(ChainId, "bitcoin");
    }

    #[test]
    fn address_id_smoke() {
        string_id_smoke!(AddressId, "bc1qxy2k");
    }

    #[test]
    fn string_id_eq_semantics() {
        // PartialEq 同值相等
        assert_eq!(PoolId::new("x"), PoolId::new("x"));
        assert_ne!(PoolId::new("a"), PoolId::new("b"));
    }

    #[test]
    fn task_id_new_default_display_and_serde() {
        let t1 = TaskId::new();
        let t2 = TaskId::default();
        // 两次 new 生成不同 UUID（极小概率碰撞，不严格断言不等，只断言 Display 非空）
        let s1 = format!("{t1}");
        let s2 = format!("{t2}");
        assert!(!s1.is_empty());
        assert!(!s2.is_empty());

        // Display == 内部 Uuid 字符串
        assert_eq!(s1, format!("{}", t1.0));

        // serde 往返
        let json = serde_json::to_string(&t1).unwrap();
        // Uuid 序列化为 JSON 字符串
        let back: TaskId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, t1);

        // Copy 派生：移动不消耗（TaskId 是 Copy）
        let t3 = t1;
        let _ = t1; // 仍可用 -> 证 Copy
        assert_eq!(t3, t1);

        let _ = format!("{t1:?}");
    }

    #[test]
    fn task_id_explicit_uuid_serde() {
        // 用显式 Uuid 构造 + serde 往返，确保确定性值也能序列化对称
        let u = uuid::Uuid::new_v4();
        let t = TaskId(u);
        let s = serde_json::to_string(&t).unwrap();
        let back: TaskId = serde_json::from_str(&s).unwrap();
        assert_eq!(back.0, u);
    }
}

// ============================================================================
// error.rs —— Display 实现（thiserror 派生体默认无单测调用）
// ============================================================================

mod error_tests {
    use super::*;

    #[test]
    fn core_error_serde_display() {
        // 触发 Serde 变体：构造一个 serde_json 错误（解析坏 JSON）
        let bad: Result<serde_json::Value, _> = serde_json::from_str("{bad}");
        let json_err = bad.unwrap_err();
        let e = CoreError::Serde(json_err);
        assert!(
            format!("{e}").starts_with("序列化错误:"),
            "Display 缺前缀：{}",
            e
        );
        assert!(
            format!("{e}").contains("序列化错误"),
            "Display 应包含 \"序列化错误\"：{}",
            e
        );
        // Debug 可用
        let _ = format!("{e:?}");
    }

    #[test]
    fn core_error_event_bus_display() {
        let e = CoreError::EventBus("channel closed".to_string());
        let s = format!("{e}");
        assert_eq!(s, "事件总线错误: channel closed");
    }

    #[test]
    fn core_error_internal_display() {
        let e = CoreError::Internal("boom".to_string());
        let s = format!("{e}");
        assert_eq!(s, "内部错误: boom");
    }

    #[test]
    fn core_error_from_serde_via_question_mark() {
        // 验证 #[from] 转换：serde_json::Error -> CoreError 自动转
        fn fallible() -> Result<(), CoreError> {
            let _: serde_json::Value = serde_json::from_str("{bad")?;
            Ok(())
        }
        let e = fallible().unwrap_err();
        assert!(matches!(e, CoreError::Serde(_)));
        let s = format!("{e}");
        assert!(s.starts_with("序列化错误:"));
    }

    #[test]
    fn core_error_all_variants_collect() {
        // 综合断言三个变体 Display 前缀，确保无一遗漏
        let variants: Vec<String> = vec![
            format!("{}", CoreError::EventBus("x".to_string())),
            format!("{}", CoreError::Internal("y".to_string())),
        ];
        assert!(variants[0].starts_with("事件总线错误:"));
        assert!(variants[1].starts_with("内部错误:"));
    }
}

// ============================================================================
// 跨类型综合：把 DTO + ID 一起塞进 serde_json::Value，证整体可序列化（覆盖跨用路径）
// ============================================================================

#[test]
fn composite_dto_json_roundtrip() {
    let node = NodeInfo {
        node_id: NodeId::new("node-7"),
        role: NodeRole::Follower,
        version: "1.2.3".to_string(),
        arch: "aarch64".to_string(),
        endpoints: vec!["192.168.1.7:9000".to_string()],
        health: Health::Healthy,
    };
    let report = HealthReport {
        health: Health::Healthy,
        message: Some("ok".to_string()),
        timestamp: chrono::Utc::now(),
    };
    let composite = json!({
        "node": node,
        "report": report,
        "pool": PoolId::new("tank"),
        "vm": VmId::new("vm-9"),
        "task": TaskId::new(),
        "quota": ResourceQuota {
            cpu_cores: Some(1.0),
            memory_bytes: Some(4096),
            io_bps_limit: None,
        },
        "capacity": Capacity {
            used_bytes: 10,
            total_bytes: 100,
        },
    });
    // 整体可序列化为 JSON 字符串
    let s = serde_json::to_string(&composite).unwrap();
    assert!(s.contains("\"node\":\"node-7\"") || s.contains("\"node_id\":\"node-7\""));
    assert!(s.contains("\"pool\":\"tank\""));
    assert!(s.contains("\"vm\":\"vm-9\""));
    // 反序列化回 Value（证写入可读回）
    let back: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(back["pool"], "tank");
}

// 引入未在 use 列出的 Capacity（与 composite 测试配套；放文件尾避免顶部噪声）
use os_core::Capacity;
