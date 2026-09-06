# discover-agent 任务队列

## 待办（由主代理分派）
- [ ] discover-T-004：依赖注册后实现真实 MdnsDiscovery（mdns-sd 组播，依赖硬阻塞）
- [ ] discover-T-005：依赖注册后实现 MtlsPeerAuthenticator（rustls mTLS + os-security CertManager 协同）
- [ ] discover-T-006：beacon verify_beacon_signature 接入 ed25519-dalek 真实验签
- [ ] discover-T-007：mTLS 证书指纹校验（SHA-256）集成测

## 进行中
（无）

## 完成
- [x] discover-T-001：节点能力模型 + HA 资格检测纯算法（2026-08-05）
- [x] discover-T-002：beacon 防伪签名 challenge/nonce 生成与比对（2026-08-05）
- [x] discover-T-003：联邦决策状态机 + DefaultFederationPolicy + MdnsDiscovery 骨架 + Mock 三件套（2026-08-05，本批 3）

## 阻塞
（无；T-004/005/006 阻塞于第三方依赖在 workspace 注册，由主代理统一推进）
