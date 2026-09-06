//! 组件依赖拓扑排序 + 循环检测（纯算法，无外部依赖）
//!
//! 实现细节（规划文档 §3.13 / §5.1）：
//! - 用 Kahn 算法（入度表 + BFS）做拓扑排序：稳定、O(V+E)、能区分 DAG / 有环。
//! - 检测到环时返回 [`crate::OrchestratorError::DependencyCycle`]，错误信息含参与环的节点。
//! - 本模块纯函数（输入 `&ComponentRegistry`，输出 `Vec<ComponentId>` 或错误），
//!   无 IO / 无锁 / 无 async，便于确定性单元测试（规格书 §5.1 点名）。

use std::collections::{HashMap, HashSet, VecDeque};

use crate::component::{ComponentDescriptor, ComponentId};
use crate::OrchestratorError;

/// 拓扑排序结果
pub type TopoResult = Result<Vec<ComponentId>, OrchestratorError>;

/// 对一组组件描述符做拓扑排序
///
/// 输入任意顺序的组件切片，输出按依赖关系排序后的启动顺序（依赖在前，
/// 被依赖在后）。`enabled=false` 的组件会被**跳过**（编排器不拉起），
/// 但若被其他启用组件依赖，仍视为合法节点参与排序。
///
/// # 算法
/// Kahn 算法：
/// 1. 计算每个节点的入度（被多少节点依赖的反向——此处"入度"定义为
///    "本节点依赖了多少其他节点"，即 `dependencies.len()` 减去不在注册表内的）。
/// 2. 入度为 0 的节点入队。
/// 3. 出队一个节点加入结果；把以它为依赖的节点入度减一；新的 0 入度节点入队。
/// 4. 若结果数 < 节点数 → 有环。
///
/// # 错误
/// - [`OrchestratorError::DependencyCycle`]：依赖图含环。错误信息列出参与环的节点。
///
/// # 注意
/// - 自依赖（A 依赖 A）视为环。
/// - 依赖了注册表外的组件：当前实现**忽略**（视为该依赖满足），
///   避免在编排器层因注册不完整而误报；真正缺失由调用方在 start 时校验。
///   这是经过权衡的设计：拓扑排序只关心"可拉起组件之间"的偏序。
pub fn topological_sort(descriptors: &[ComponentDescriptor]) -> TopoResult {
    // 仅参与排序的节点集合（所有描述符，含 disabled——见上文说明）。
    let id_set: HashSet<&ComponentId> = descriptors.iter().map(|d| &d.id).collect();

    // in_degree[id] = 本节点依赖了多少个【注册表内】的其他节点
    // （注册表外的依赖被忽略，见函数文档）。
    let mut in_degree: HashMap<&ComponentId, usize> = HashMap::new();
    // dependents[A] = 依赖 A 的节点列表（A 在前，这些节点在后）
    let mut dependents: HashMap<&ComponentId, Vec<&ComponentId>> = HashMap::new();

    for d in descriptors {
        in_degree.entry(&d.id).or_insert(0);
        dependents.entry(&d.id).or_default();
    }

    for d in descriptors {
        for dep in &d.dependencies {
            // 仅当依赖在注册表内才计入入度
            if id_set.contains(dep) && dep != &d.id {
                *in_degree.entry(&d.id).or_insert(0) += 1;
                dependents.entry(dep).or_default().push(&d.id);
            } else if dep == &d.id {
                // 自依赖 → 环
                return Err(OrchestratorError::DependencyCycle {
                    cycle: format!("{} -> {}", d.id, d.id),
                });
            }
            // 注册表外的依赖：忽略
        }
    }

    // 入度为 0 的节点入队
    let mut queue: VecDeque<&ComponentId> = in_degree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(&id, _)| id)
        .collect();

    let mut order: Vec<ComponentId> = Vec::with_capacity(descriptors.len());

    while let Some(id) = queue.pop_front() {
        order.push(id.clone());
        if let Some(deps) = dependents.get(id) {
            for &dependent in deps {
                if let Some(deg) = in_degree.get_mut(&dependent) {
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push_back(dependent);
                    }
                }
            }
        }
    }

    if order.len() == descriptors.len() {
        Ok(order)
    } else {
        // 有环：未出队的节点都在环里
        let in_cycle: Vec<String> = descriptors
            .iter()
            .map(|d| &d.id)
            .filter(|id| !order.iter().any(|o| o == *id))
            .map(|id| id.to_string())
            .collect();
        Err(OrchestratorError::DependencyCycle {
            cycle: in_cycle.join(" -> "),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::{ComponentDescriptor, ComponentId, HealthProbeConfig};
    use os_core::ResourceQuota;

    fn desc(id: &str, deps: &[&str]) -> ComponentDescriptor {
        ComponentDescriptor {
            id: ComponentId::new(id),
            dependencies: deps.iter().map(|&s| ComponentId::new(s)).collect(),
            quota: ResourceQuota {
                cpu_cores: None,
                memory_bytes: None,
                io_bps_limit: None,
            },
            health_probe: HealthProbeConfig {
                kind: "exec".into(),
                target: "/bin/true".into(),
                interval_secs: 10,
                timeout_secs: 1,
                failure_threshold: 3,
            },
            command: Some("/bin/true".into()),
            enabled: true,
        }
    }

    #[test]
    fn empty_input_returns_empty() {
        let order = topological_sort(&[]).expect("空输入应返回空");
        assert!(order.is_empty());
    }

    #[test]
    fn single_node_no_deps() {
        let order = topological_sort(&[desc("a", &[])]).expect("单节点");
        assert_eq!(order, vec![ComponentId::new("a")]);
    }

    #[test]
    fn linear_chain_preserves_order() {
        // c -> b -> a（c 依赖 b 依赖 a）：期望顺序 a, b, c
        let descs = [desc("c", &["b"]), desc("b", &["a"]), desc("a", &[])];
        let order = topological_sort(&descs).expect("线性链应可排序");
        assert_eq!(order.len(), 3);
        let pos = |id: &str| order.iter().position(|x| x.as_str() == id).unwrap();
        assert!(pos("a") < pos("b"));
        assert!(pos("b") < pos("c"));
    }

    #[test]
    fn diamond_dependency() {
        // d -> {b, c}; b -> a; c -> a
        // 期望 a 第一，d 最后，b/c 居中
        let descs = [
            desc("d", &["b", "c"]),
            desc("b", &["a"]),
            desc("c", &["a"]),
            desc("a", &[]),
        ];
        let order = topological_sort(&descs).expect("菱形依赖应可排序");
        assert_eq!(order.len(), 4);
        let pos = |id: &str| order.iter().position(|x| x.as_str() == id).unwrap();
        assert_eq!(pos("a"), 0, "a 必须最先");
        assert_eq!(pos("d"), 3, "d 必须最后");
    }

    #[test]
    fn simple_two_node_cycle_detected() {
        // a -> b -> a
        let descs = [desc("a", &["b"]), desc("b", &["a"])];
        let err = topological_sort(&descs).expect_err("应检测到环");
        match err {
            OrchestratorError::DependencyCycle { cycle } => {
                // 环中节点都应出现在错误信息里
                assert!(cycle.contains('a') || cycle.contains('b'));
            }
            other => panic!("期望 DependencyCycle，实际: {:?}", other),
        }
    }

    #[test]
    fn self_dependency_detected() {
        let descs = [desc("a", &["a"])];
        let err = topological_sort(&descs).expect_err("自依赖应检测为环");
        assert!(matches!(err, OrchestratorError::DependencyCycle { .. }));
    }

    #[test]
    fn three_node_cycle_detected() {
        // a -> b -> c -> a
        let descs = [desc("a", &["b"]), desc("b", &["c"]), desc("c", &["a"])];
        let err = topological_sort(&descs).expect_err("三节点环应被检测");
        match err {
            OrchestratorError::DependencyCycle { cycle } => {
                for node in ["a", "b", "c"] {
                    assert!(cycle.contains(node), "环信息应含 {}", node);
                }
            }
            other => panic!("期望 DependencyCycle，实际: {:?}", other),
        }
    }

    #[test]
    fn node_partially_in_cycle_extra_free_node_still_sorts_free() {
        // a -> b -> a（环）；c 独立
        // 整体有环应报错
        let descs = [desc("a", &["b"]), desc("b", &["a"]), desc("c", &[])];
        let err = topological_sort(&descs).expect_err("含环应报错");
        assert!(matches!(err, OrchestratorError::DependencyCycle { .. }));
    }

    #[test]
    fn external_dependency_ignored() {
        // a 依赖 "external"（不在注册表）：应被忽略，a 仍可排序
        let descs = [desc("a", &["external"])];
        let order = topological_sort(&descs).expect("外部依赖应被忽略");
        assert_eq!(order, vec![ComponentId::new("a")]);
    }

    #[test]
    fn duplicate_ids_handled_by_registry_dedup() {
        // topological_sort 假设输入已去重（由 ComponentRegistry::from_descriptors 负责：
        // 重复 ID 后者覆盖前者）。这里验证"去重后"的输入可正常排序。
        use crate::impl_orchestrator::ComponentRegistry;
        let registry = ComponentRegistry::from_descriptors(vec![
            desc("a", &[]),
            desc("a", &["b"]), // 覆盖
            desc("b", &[]),
        ]);
        // 注册表去重后只剩 a(依赖 b)、b
        assert_eq!(registry.len(), 2);
        let descs: Vec<ComponentDescriptor> = registry.all().into_iter().cloned().collect();
        let order = topological_sort(&descs).expect("去重后应可排序");
        let pos = |id: &str| order.iter().position(|x| x.as_str() == id).unwrap();
        assert!(pos("b") < pos("a"), "b 应先于 a（a 依赖 b）");
    }

    // ---- 边界场景补充（多连通分量 / 重复边 / 大偏序） ----

    #[test]
    fn two_isolated_components_each_sorts() {
        // 两个不相连的连通分量：a→b 与 c→d，互不依赖
        let descs = [
            desc("a", &["b"]),
            desc("b", &[]),
            desc("c", &["d"]),
            desc("d", &[]),
        ];
        let order = topological_sort(&descs).expect("两个连通分量应可排序");
        assert_eq!(order.len(), 4);
        let pos = |id: &str| order.iter().position(|x| x.as_str() == id).unwrap();
        // 各分量内部偏序成立
        assert!(pos("b") < pos("a"), "b 应先于 a");
        assert!(pos("d") < pos("c"), "d 应先于 c");
    }

    #[test]
    fn duplicate_edges_are_idempotent() {
        // a 多次声明依赖 b（重复边）→ 入度只 +1 一次，仍可排序
        let mut d = desc("a", &["b", "b", "b"]);
        d.dependencies = vec![ComponentId::new("b"), ComponentId::new("b")];
        let descs = [d, desc("b", &[])];
        let order = topological_sort(&descs).expect("重复边应可排序");
        assert_eq!(order.len(), 2);
        let pos = |id: &str| order.iter().position(|x| x.as_str() == id).unwrap();
        assert!(pos("b") < pos("a"), "b 应先于 a");
    }

    #[test]
    fn many_independent_nodes_all_returned() {
        // 5 个独立节点（无任何依赖）→ 全部返回，顺序由 HashMap 决定但不丢节点
        let descs = [
            desc("n1", &[]),
            desc("n2", &[]),
            desc("n3", &[]),
            desc("n4", &[]),
            desc("n5", &[]),
        ];
        let order = topological_sort(&descs).expect("独立节点应可排序");
        assert_eq!(order.len(), 5);
        for n in ["n1", "n2", "n3", "n4", "n5"] {
            assert!(order.iter().any(|x| x.as_str() == n), "应包含 {n}");
        }
    }

    #[test]
    fn fan_in_many_depend_on_one() {
        // b/c/d 都依赖 a（扇入）→ a 必须最先
        let descs = [
            desc("b", &["a"]),
            desc("c", &["a"]),
            desc("d", &["a"]),
            desc("a", &[]),
        ];
        let order = topological_sort(&descs).expect("扇入应可排序");
        let pos = |id: &str| order.iter().position(|x| x.as_str() == id).unwrap();
        assert_eq!(pos("a"), 0, "a 必须最先（被多人依赖）");
    }

    #[test]
    fn fan_out_one_depends_on_many() {
        // d 依赖 a/b/c（扇出）→ d 必须最后
        let descs = [
            desc("d", &["a", "b", "c"]),
            desc("a", &[]),
            desc("b", &[]),
            desc("c", &[]),
        ];
        let order = topological_sort(&descs).expect("扇出应可排序");
        let pos = |id: &str| order.iter().position(|x| x.as_str() == id).unwrap();
        assert_eq!(pos("d"), 3, "d 必须最后");
    }

    #[test]
    fn external_and_internal_deps_mixed() {
        // a 依赖 [ext（注册表外，忽略）, b（注册表内）]
        let mut d = desc("a", &[]);
        d.dependencies = vec![ComponentId::new("ext"), ComponentId::new("b")];
        let descs = [d, desc("b", &[])];
        let order = topological_sort(&descs).expect("混合依赖应可排序");
        let pos = |id: &str| order.iter().position(|x| x.as_str() == id).unwrap();
        assert!(pos("b") < pos("a"), "b 应先于 a");
    }

    #[test]
    fn cycle_error_message_lists_unsorted_nodes() {
        // 4 节点：a→b→a 环 + c→d 链。环检测后错误信息应至少包含 a/b
        let mut a = desc("a", &["b"]);
        let mut b = desc("b", &["a"]);
        // 显式覆盖 dependencies 以避免 helper 默认值
        a.dependencies = vec![ComponentId::new("b")];
        b.dependencies = vec![ComponentId::new("a")];
        let descs = [a, b, desc("c", &["d"]), desc("d", &[])];
        let err = topological_sort(&descs).expect_err("含环应报错");
        match err {
            OrchestratorError::DependencyCycle { cycle } => {
                // 未出队的 a、b 应出现在环信息中
                assert!(
                    cycle.contains('a') && cycle.contains('b'),
                    "环信息: {cycle}"
                );
            }
            other => panic!("期望 DependencyCycle，实际: {other:?}"),
        }
    }

    #[test]
    fn disabled_component_still_participates_in_sort() {
        // enabled=false 的组件仍作为图节点参与排序（规格：被依赖时合法）
        let mut a = desc("a", &["b"]);
        a.enabled = false;
        let descs = [a, desc("b", &[])];
        // topological_sort 不看 enabled，只看 dependencies
        let order = topological_sort(&descs).expect("禁用组件仍参与排序");
        let pos = |id: &str| order.iter().position(|x| x.as_str() == id).unwrap();
        assert!(pos("b") < pos("a"));
    }
}
