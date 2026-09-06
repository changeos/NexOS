// =============================================================================
// fedUpgradeDecision 决策矩阵测试（零依赖，node 直跑；仓库 web 端无 vitest，
// 故用 node:assert 断言 + 末尾汇总。运行：
// `cd crates/os-api/web && npx tsc src/utils/fedImport.ts
// src/utils/fedImport.test.ts --ignoreConfig --outDir /tmp/fedtest --module
// commonjs --target es2022 --esModuleInterop --skipLibCheck --strict --types
// node && node /tmp/fedtest/fedImport.test.js`。）
// =============================================================================
import { deepStrictEqual, strictEqual } from 'node:assert/strict';
import { fedUpgradeDecision } from './fedImport';

let passed = 0;
function check(name: string, fn: () => void): void {
  fn();
  passed += 1;
  console.log(`ok - ${name}`);
}

// —— 用户实测缺陷场景：0.1.16 前导入的旧行（无 via_node / 无 key / 无模型）
//    + 联邦条目（source_node_id + model_name + 脱敏 key）→ 升级为经源节点
//    中继（写 via_node + models），绝不写脱敏 key，提示手动补填 ——
check('旧行(直连/无key) + 脱敏key条目 → 写 via_node+models、不写 key、需手动补填', () => {
  const d = fedUpgradeDecision(
    { via_node: '', has_api_key: false, models: [] },
    {
      source_node_id: '0x' + 'ab'.repeat(33),
      server_config: { model_name: 'qwen3.5-9b' },
      access_info: { api_key: 'sk-a***3456' },
    },
  );
  deepStrictEqual(d.patch, { via_node: '0x' + 'ab'.repeat(33), models: ['qwen3.5-9b'] });
  strictEqual(d.upgraded, true);
  strictEqual(d.keyMasked, true);
  strictEqual(d.needsManualKey, true);
});

// —— 旧行 + 新凭据（明文 key）→ 更新字段集含 api_key ——
check('旧行(无key) + 明文key条目 → 补丁含 via_node+api_key+models', () => {
  const d = fedUpgradeDecision(
    { via_node: '', has_api_key: false, models: [] },
    {
      source_node_id: '0xnode66hex',
      server_config: { model_name: 'm1' },
      access_info: { api_key: 'sk-plain-987654' },
    },
  );
  deepStrictEqual(d.patch, {
    via_node: '0xnode66hex',
    api_key: 'sk-plain-987654',
    models: ['m1'],
  });
  strictEqual(d.upgraded, true);
  strictEqual(d.keyMasked, false);
  strictEqual(d.needsManualKey, false);
});

// —— 旧行已有 key + 脱敏条目 → 绝不覆盖已有 key（补丁不含 api_key）——
check('旧行(有key) + 脱敏key条目 → 不覆盖 key', () => {
  const d = fedUpgradeDecision(
    { via_node: '', has_api_key: true, models: ['m1'] },
    { source_node_id: '0xn', access_info: { api_key: '****' } },
  );
  strictEqual('api_key' in d.patch, false, '脱敏 key 不得进补丁');
  deepStrictEqual(d.patch, { via_node: '0xn' });
  strictEqual(d.needsManualKey, false, '旧行已有 key 无需手动补填');
});

// —— 空 key / 纯空白条目 → 同样不写 ——
check('条目 key 为空/空白 → 不写 api_key', () => {
  for (const key of ['', '   ']) {
    const d = fedUpgradeDecision(
      { via_node: '', has_api_key: true, models: [] },
      { access_info: { api_key: key } },
    );
    strictEqual('api_key' in d.patch, false);
    strictEqual(d.keyMasked, false);
  }
});

// —— 0.1.16 后已升级过的行再导入 → 无新增凭据，补丁空、只切换 ——
check('已中继行(同 via_node/同模型) + 脱敏key → 补丁空、不升级', () => {
  const node = '0x' + 'cd'.repeat(33);
  const d = fedUpgradeDecision(
    { via_node: node, has_api_key: true, models: ['qwen3.5-9b'] },
    {
      source_node_id: node,
      server_config: { model_name: 'qwen3.5-9b' },
      access_info: { api_key: 'sk-a***3456' },
    },
  );
  deepStrictEqual(d.patch, {});
  strictEqual(d.upgraded, false);
  strictEqual(d.needsManualKey, false);
});

// —— 明文 key 无法与旧行比对（列表只有脱敏串）→ 提供即覆盖（幂等）——
check('旧行(有key) + 明文key条目 → 仍写 api_key（无法比对，提供即覆盖）', () => {
  const d = fedUpgradeDecision(
    { via_node: '0xn', has_api_key: true, models: ['m'] },
    { server_config: { model_name: 'm' }, access_info: { api_key: 'sk-new' } },
  );
  deepStrictEqual(d.patch, { api_key: 'sk-new' });
  strictEqual(d.upgraded, true);
});

// —— 条目缺字段（无 source_node_id / 无 model_name）→ 对应字段不写 ——
check('条目无来源节点/无模型名 → 只写明文 key', () => {
  const d = fedUpgradeDecision(
    { via_node: '', has_api_key: false, models: [] },
    { access_info: { api_key: 'sk-k' } },
  );
  deepStrictEqual(d.patch, { api_key: 'sk-k' });
});

// —— 空白 source_node_id → 不写入（trim 后判空）——
check('空白 source_node_id → 不写 via_node', () => {
  const d = fedUpgradeDecision(
    { via_node: '', has_api_key: false, models: [] },
    { source_node_id: '   ', server_config: { model_name: 'm' } },
  );
  strictEqual('via_node' in d.patch, false);
  deepStrictEqual(d.patch, { models: ['m'] });
});

// —— 旧行模型清单不同（含连通测试回填的多模型）→ 置为 [model_name]
//    （与一键新建导入同形）——
check('旧行模型=[a,b] + 条目 model_name=q → models=[q]', () => {
  const d = fedUpgradeDecision(
    { via_node: '', has_api_key: false, models: ['a', 'b'] },
    { server_config: { model_name: 'q' } },
  );
  deepStrictEqual(d.patch, { models: ['q'] });
});

// —— via_node 变更（换源节点）→ 覆盖为新的 ——
check('旧行 via_node=旧 + 条目=新 → 覆盖为新 via_node', () => {
  const d = fedUpgradeDecision(
    { via_node: '0xold', has_api_key: false, models: [] },
    { source_node_id: '0xnew' },
  );
  deepStrictEqual(d.patch, { via_node: '0xnew' });
});

// —— 最小旧行字面量（可选字段缺省）也能决策 ——
check('最小旧行字面量 {} + 全空条目 → 补丁空', () => {
  const d = fedUpgradeDecision({}, {});
  deepStrictEqual(d.patch, {});
  strictEqual(d.upgraded, false);
  strictEqual(d.needsManualKey, false);
});

console.log(`\nfedUpgradeDecision: ${passed} passed, 0 failed`);
