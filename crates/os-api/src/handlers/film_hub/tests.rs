//! FilmHub 单元测试（mock 注入同 film.rs 惯例：fake vLLM TCP / 渠道 HTTP
//! mock / 假生图脚本 / 假 ffmpeg——绝不真调外部模型、不装真实 ffmpeg）。

use super::*;
use crate::gateway::RouteHandler;
use std::io::{Read, Write};
use std::sync::Mutex as StdMutex;

// ---------------------------------------------------------------------------
// 通用 fixture（与 film.rs tests 同款手法）
// ---------------------------------------------------------------------------

fn temp_dir_for(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nexos-filmhub-{test}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(unix)]
fn fake_exec(dir: &Path, name: &str, content: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    let mut perm = std::fs::metadata(&path).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&path, perm).unwrap();
    path
}

fn handler_at(test: &str) -> (FilmRouteHandler, PathBuf) {
    let dir = temp_dir_for(test);
    let h = FilmRouteHandler::with_db_path(dir.join("film.db").to_str().unwrap())
        .with_root_dir(dir.join("root").to_str().unwrap());
    (h, dir)
}

fn get_req(path: &str) -> ApiRequest {
    ApiRequest {
        method: HttpMethod::Get,
        path: path.into(),
        headers: serde_json::json!({}),
        body: Value::Null,
        auth: None,
    }
}

fn post_req(path: &str, body: Value) -> ApiRequest {
    ApiRequest {
        method: HttpMethod::Post,
        path: path.into(),
        headers: serde_json::json!({}),
        body,
        auth: None,
    }
}

fn put_req(path: &str, body: Value) -> ApiRequest {
    ApiRequest {
        method: HttpMethod::Put,
        path: path.into(),
        headers: serde_json::json!({}),
        body,
        auth: None,
    }
}

fn delete_req(path: &str) -> ApiRequest {
    ApiRequest {
        method: HttpMethod::Delete,
        path: path.into(),
        headers: serde_json::json!({}),
        body: Value::Null,
        auth: None,
    }
}

async fn create_project(h: &FilmRouteHandler, ratio: &str) -> (String, String) {
    let resp = h
        .handle(post_req(
            "/api/v1/film/projects",
            serde_json::json!({
                "title": "测试 影片!",
                "idea": "一只猫在霓虹城市里寻找回家路",
                "ratio": ratio,
                "style_hint": "赛博朋克",
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201, "建项目失败: {resp:?}");
    (
        resp.body["id"].as_str().unwrap().to_string(),
        resp.body["dir"].as_str().unwrap().to_string(),
    )
}

fn seed_script(dir: &str, shots: Vec<Value>) {
    let file = serde_json::json!({
        "shots": shots,
        "generated_by": "test-seed",
        "created_at": "2026-09-06T00:00:00+08:00",
    });
    std::fs::write(
        format!("{dir}/script.json"),
        serde_json::to_string_pretty(&file).unwrap(),
    )
    .unwrap();
}

fn shot_json(n: u32, line: &str, dur: u32) -> Value {
    serde_json::json!({
        "shot": n,
        "desc": format!("镜头{n}画面"),
        "image_prompt": format!("镜头{n}关键帧"),
        "video_prompt": format!("镜头{n}运动"),
        "line": line,
        "duration_secs": dur,
    })
}

async fn wait_task(h: &FilmRouteHandler, id: &str) -> Value {
    for _ in 0..400 {
        let resp = h
            .handle(get_req(&format!("/api/v1/film/tasks/{id}")))
            .await
            .unwrap();
        let status = resp.body["status"].as_str().unwrap_or("");
        if status == "done" || status == "error" {
            return resp.body;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("任务 {id} 未在 10s 内到达终态");
}

async fn run_stage(h: &FilmRouteHandler, path: &str, body: Value) -> (Value, String) {
    let resp = h.handle(post_req(path, body)).await.unwrap();
    assert_eq!(resp.status, 202, "阶段应 202: {resp:?}");
    let id = resp.body["id"].as_str().unwrap().to_string();
    let task = wait_task(h, &id).await;
    (task, id)
}

fn spawn_mock_upstream(responses: Vec<Vec<u8>>) -> (u16, Arc<StdMutex<Vec<String>>>) {
    let hits: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(vec![]));
    let hits2 = Arc::clone(&hits);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for body in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buf);
                        if let Some(hend) = text.find("\r\n\r\n") {
                            let cl = text[..hend]
                                .lines()
                                .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                                .and_then(|l| l.split(':').nth(1))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            if buf.len() >= hend + 4 + cl {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            hits2
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf).into_owned());
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
            let _ = stream.flush();
        }
    });
    (port, hits)
}

fn chat_response(content: &str) -> Vec<u8> {
    serde_json::json!({
        "id": "chatcmpl-filmhub-test",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 30, "completion_tokens": 50, "total_tokens": 80},
    })
    .to_string()
    .into_bytes()
}

async fn seed_channel(
    gw: &super::super::api_gateway::ApiGatewayRouteHandler,
    base_url: &str,
) -> String {
    let resp = gw
        .handle(post_req(
            "/api/v1/gateway/channels",
            serde_json::json!({
                "name": "filmhub-test-渠道",
                "provider": "openai",
                "base_url": base_url,
                "api_key": "sk-upstream-test",
                "models": ["test-model"],
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201, "种渠道失败: {resp:?}");
    resp.body["id"].as_str().unwrap().to_string()
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn read_json(path: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn png_bytes(payload: &[u8]) -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    v.extend_from_slice(payload);
    v
}

fn seed_story_md(dir: &str, body: &str) {
    let mut fm = BTreeMap::new();
    fm.insert("words".to_string(), body.chars().count().to_string());
    fm.insert("summary".to_string(), "猫寻找回家路".to_string());
    std::fs::write(format!("{dir}/hub/story/story.md"), render_doc(&fm, body)).unwrap();
}

fn activity_list(dir: &str) -> Vec<Value> {
    read_json(&format!("{dir}/hub/activity.json"))
        .as_array()
        .cloned()
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 纯函数：slug / front-matter / 百分号解码
// ---------------------------------------------------------------------------

#[test]
fn slugify_keeps_cjk_and_folds_punctuation() {
    assert_eq!(slugify("小明"), "小明");
    assert_eq!(slugify(" 小明 & 小红 v2 "), "小明-小红-v2");
    assert_eq!(slugify("!!!"), "", "全符号 slug 为空（调用面须兜底）");
    assert_eq!(slugify("a/b\\c"), "a-b-c", "路径分隔符折叠");
}

#[test]
fn front_matter_roundtrip_and_quote_escaping() {
    let mut fm = BTreeMap::new();
    fm.insert("title".to_string(), "落日: 航线 #1".to_string());
    fm.insert("summary".to_string(), "一句话梗概".to_string());
    let doc = render_doc(&fm, "正文第一幕……");
    let (parsed, body) = split_front_matter(&doc);
    assert_eq!(
        parsed.get("title").unwrap(),
        "落日: 航线 #1",
        "含冒号/井号值加引号往返: {doc}"
    );
    assert_eq!(parsed.get("summary").unwrap(), "一句话梗概");
    assert!(body.contains("正文第一幕"));
    // 无 front-matter 原文直通
    let (m2, b2) = split_front_matter("纯正文");
    assert!(m2.is_empty());
    assert_eq!(b2, "纯正文");
}

#[test]
fn percent_decode_segments() {
    assert_eq!(percent_decode("%E5%B0%8F%E6%98%8E"), "小明");
    assert_eq!(percent_decode("plain-name"), "plain-name");
    assert_eq!(percent_decode("100%"), "100%", "非法序列原样保留");
}

// ---------------------------------------------------------------------------
// 纯函数：提示词三份（剧情 / 分镜 / 提取）
// ---------------------------------------------------------------------------

#[test]
fn story_prompt_double_anchor_and_source_block() {
    let p = build_story_prompt("猫回家", "16:9", Some("水墨"), None);
    assert!(p.matches("猫回家").count() >= 2, "创意首尾双锚定: {p}");
    assert!(p.contains("禁止更换题材") && p.contains("禁止另编"));
    assert!(p.contains("【第一幕】") && p.contains("3 到 6 幕"));
    assert!(!p.contains("改编原文"));
    let p2 = build_story_prompt("猫回家", "16:9", None, Some("原文内容 ABC"));
    assert!(
        p2.contains("【改编原文】") && p2.contains("原文内容 ABC"),
        "{p2}"
    );
    assert!(p2.contains("改编浓缩"));
}

#[test]
fn storyboard_prompt_reads_story_and_casting_slots() {
    let story = "【第一幕】灯塔下的猫。\n【第二幕】屋顶追逐。";
    let p = build_storyboard_prompt(Some(story), "创意X", "9:16", None, &[]);
    assert!(p.contains("【剧情】") && p.contains("灯塔下的猫"), "{p}");
    assert!(p.contains("从剧情逐幕分析"));
    assert!(p.contains("禁止更换题材"), "题材硬约束同款: {p}");
    for f in [
        "\"characters\"",
        "\"props\"",
        "\"pets\"",
        "\"scenes\"",
        "\"actions\"",
    ] {
        assert!(p.contains(f), "casting 空槽字段 {f}: {p}");
    }
    assert!(p.contains("定妆阶段建定妆对象"), "引用语义说明: {p}");
    // 无剧情回落【创意】（旧 script 兼容别名路径）
    let p2 = build_storyboard_prompt(None, "创意X", "16:9", None, &[]);
    assert!(p2.contains("【创意】") && p2.contains("创意X"), "{p2}");
    // 角色表注入
    let p3 = build_storyboard_prompt(None, "创意X", "16:9", None, &["小明".into(), "小红".into()]);
    assert!(p3.contains("【角色表】") && p3.contains("小明"), "{p3}");
}

#[test]
fn extract_prompt_defines_six_classes_and_frequency_rule() {
    let shots = vec![serde_json::from_value::<ScriptShot>(shot_json(1, "", 5)).unwrap()];
    let p = build_extract_prompt("剧情正文", &shots);
    for k in [
        "characters",
        "weapons",
        "pets",
        "formations",
        "actions",
        "scenes",
    ] {
        assert!(p.contains(&format!("- {k}：")), "六类定义 {k}: {p}");
    }
    assert!(p.contains("frequency 必须是整数"), "统计要求: {p}");
    assert!(p.contains("出场的镜头数"), "{p}");
    assert!(p.contains("只输出一个 JSON 对象"), "{p}");
}

// ---------------------------------------------------------------------------
// 纯函数：提取解析容错 / 计价 / ownership 校验
// ---------------------------------------------------------------------------

#[test]
fn parse_extraction_tolerant_shapes() {
    let raw = r#"{"characters":[{"name":"小明","desc":"黑发","frequency":3,"reason":"主角"}],"weapons":[{"name":"长刀","frequency":"2次"}],"pets":[],"formations":[],"actions":[{"name":"拔剑"}],"scenes":[{"name":"灯塔顶","frequency":1}]}"#;
    let v = parse_extraction(raw).expect("纯对象应可解析");
    assert_eq!(v["characters"][0]["name"], "小明");
    assert_eq!(v["characters"][0]["frequency"], 3);
    assert_eq!(v["weapons"][0]["frequency"], 2, "字符串'2次'归一");
    assert_eq!(v["actions"][0]["frequency"], 0, "缺省 frequency=0");
    assert_eq!(v["scenes"][0]["name"], "灯塔顶");
    // 围栏 + 散文包裹
    let fenced = format!("好的：\n```json\n{raw}\n```\n完成");
    assert!(parse_extraction(&fenced).is_ok());
    let prose = format!("前情 {} 后记", raw);
    assert!(parse_extraction(&prose).is_ok());
    // <think> 剥离
    let think = format!("<think>噪声 {{}}</think>{raw}");
    assert!(parse_extraction(&think).is_ok());
    // 全空 / 垃圾拒绝
    assert!(
        parse_extraction("{\"characters\":[]}").is_err(),
        "全空视为失败"
    );
    assert!(parse_extraction("我认为没有可定妆的对象").is_err());
    // 缺名条目丢弃
    let noname = r#"{"characters":[{"desc":"无名"}],"weapons":[]}"#;
    let v2 = parse_extraction(noname);
    assert!(v2.is_err(), "过滤无名后全空应 Err: {v2:?}");
}

#[test]
fn parse_script_shots_casting_fields_normalized() {
    let raw = r#"[{"desc":"d","image_prompt":"p","characters":[" 小明 ","小明"],"props":[" 长刀 ","","长刀"],"pets":[],"scenes":["灯塔顶"],"actions":["拔剑","拔剑"]}]"#;
    let shots = parse_script_shots(raw).unwrap();
    assert_eq!(shots[0].characters, vec!["小明".to_string()]);
    assert_eq!(
        shots[0].props,
        vec!["长刀".to_string()],
        "casting 扩展同款归一"
    );
    assert_eq!(shots[0].scenes, vec!["灯塔顶".to_string()]);
    assert_eq!(shots[0].actions, vec!["拔剑".to_string()]);
    assert!(shots[0].pets.is_empty());
}

#[test]
fn est_cost_pure_function() {
    assert_eq!(est_cost((0.0, 0.0, 0.0), 10.0, 1000), 0.0, "未配置只计量");
    assert_eq!(est_cost((1.0, 0.5, 2.0), 10.0, 500), 1.0 + 5.0 + 1.0);
    assert_eq!(est_cost((0.1, 0.0, 0.0), 0.0, 0), 0.1);
}

#[test]
fn validate_ownership_enums_and_object_keys() {
    let ok = serde_json::json!({
        "members": [{"name": "alice", "joined_at": "2026-09-06"}],
        "sections": {"story": {"owner": "alice", "claimed_at": "2026-09-06"}},
        "casting_objects": {"characters/小明": {"owner": "bob", "claimed_at": "2026-09-06"}},
    });
    assert!(validate_ownership(&ok).is_ok(), "合法形态应过");
    // sections 枚举
    let bad_section = serde_json::json!({"sections": {"budget": {"owner": "a"}}});
    let err = validate_ownership(&bad_section).unwrap_err();
    assert!(err.contains("枚举") && err.contains("budget"), "{err}");
    // casting_objects 键：type 枚举
    let bad_type = serde_json::json!({"casting_objects": {"weapons/长刀": {"owner": "a"}}});
    let err = validate_ownership(&bad_type).unwrap_err();
    assert!(err.contains("weapons") || err.contains("枚举"), "{err}");
    // 键格式：<type>/<slug>（多段拒绝）
    let bad_key = serde_json::json!({"casting_objects": {"characters/小明/x": {"owner": "a"}}});
    assert!(validate_ownership(&bad_key).is_err());
    // owner 非空
    let bad_owner = serde_json::json!({"casting_objects": {"characters/小明": {"owner": " "}}});
    assert!(validate_ownership(&bad_owner).is_err());
    // 允许认领尚未落地的对象（存在性宽容）
    let future = serde_json::json!({"casting_objects": {"scenes/未来场景": {"owner": "carol"}}});
    assert!(validate_ownership(&future).is_ok());
}

#[tokio::test]
async fn activity_ring_truncates_at_two_hundred() {
    let dir = temp_dir_for("ring");
    let root = dir.to_str().unwrap();
    std::fs::create_dir_all(root).unwrap();
    for i in 0..205 {
        append_activity(root, "alice", "files.put", &format!("f{i}")).await;
    }
    let list = read_json(&format!("{root}/activity.json"));
    let arr = list.as_array().unwrap();
    assert_eq!(arr.len(), ACTIVITY_MAX, "环形截断至 200");
    assert_eq!(arr[0]["target"], "f5", "最旧的 5 条被逐出");
    assert_eq!(arr[199]["target"], "f204");
}

// ---------------------------------------------------------------------------
// hub 树：建项初始化 + 旧项目惰性 export
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_project_initializes_hub_tree() {
    let (h, _dir) = handler_at("hub-init");
    let (id, dir) = create_project(&h, "16:9").await;
    let root = format!("{dir}/hub");
    for f in [
        "project.md",
        "README.md",
        "budget.json",
        "assets.json",
        "ownership.json",
        "activity.json",
        "story",
        "storyboard",
        "cache",
        "dist",
        "casting/characters",
    ] {
        assert!(Path::new(&format!("{root}/{f}")).exists(), "缺 {f}");
    }
    let pm = std::fs::read_to_string(format!("{root}/project.md")).unwrap();
    let (fm, body) = split_front_matter(&pm);
    assert_eq!(fm.get("title").unwrap(), "测试 影片!");
    assert_eq!(fm.get("ratio").unwrap(), "16:9");
    assert_eq!(fm.get("style_hint").unwrap(), "赛博朋克");
    assert!(
        body.contains("一只猫在霓虹城市里寻找回家路"),
        "正文=idea: {pm}"
    );
    let readme = std::fs::read_to_string(format!("{root}/README.md")).unwrap();
    let (rfm, _) = split_front_matter(&readme);
    assert_eq!(rfm.get("stage").unwrap(), "story", "新项目起步阶段");
    // files 树清单可查
    let resp = h
        .handle(get_req(&format!("/api/v1/film/projects/{id}/files")))
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    let paths: Vec<&str> = resp.body["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    for expect in ["project.md", "README.md", "budget.json", "ownership.json"] {
        assert!(paths.contains(&expect), "树清单缺 {expect}: {paths:?}");
    }
}

#[tokio::test]
async fn legacy_project_lazily_initializes_with_storyboard_and_characters() {
    let (h, dir) = handler_at("hub-lazy");
    // 手工造旧项目（无 hub/）：DB 行 + 根 script.json + 一个旧角色库角色
    let proj_dir = dir.join("root").join("film-legacy");
    std::fs::create_dir_all(&proj_dir).unwrap();
    {
        let conn = h.db.lock().unwrap();
        conn.execute(
            "INSERT INTO film_projects (id,title,idea,ratio,style_hint,status,dir,created_at,updated_at)
             VALUES ('film-legacy','老片','猫回家','16:9','水墨','scripted',?1,'2026','2026')",
            params![proj_dir.to_str().unwrap()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO film_characters (id,project_id,name,description,voice,portrait_ref,created_at,updated_at)
             VALUES ('char-1','film-legacy','小明','黑发少年，红围巾','onyx',NULL,'2026','2026')",
            [],
        )
        .unwrap();
    }
    seed_script(proj_dir.to_str().unwrap(), vec![shot_json(1, "台词", 5)]);
    // 首次调新端点（files 树清单）→ 惰性 export 初始化
    let resp = h
        .handle(get_req("/api/v1/film/projects/film-legacy/files"))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "{resp:?}");
    let root = format!("{}/hub", proj_dir.to_str().unwrap());
    // storyboard 从 script.json 平移（字段零翻译）
    let sb = read_json(&format!("{root}/storyboard/storyboard.json"));
    assert_eq!(sb["shots"].as_array().unwrap().len(), 1);
    assert_eq!(sb["shots"][0]["line"], "台词");
    // README 阶段按项目状态推断（scripted → storyboard）
    let readme = std::fs::read_to_string(format!("{root}/README.md")).unwrap();
    let (rfm, _) = split_front_matter(&readme);
    assert_eq!(rfm.get("stage").unwrap(), "storyboard");
    // 旧角色迁移为定妆对象卡
    let card = std::fs::read_to_string(format!("{root}/casting/characters/小明/card.md")).unwrap();
    let (cfm, body) = split_front_matter(&card);
    assert_eq!(cfm.get("name").unwrap(), "小明");
    assert_eq!(cfm.get("voice").unwrap(), "onyx");
    assert!(body.contains("黑发少年"));
    // 幂等：再调一次不重建（README 保留即可，无异常）
    let resp = h
        .handle(get_req("/api/v1/film/projects/film-legacy/files"))
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
}

// ---------------------------------------------------------------------------
// v0.1.36 默认树骨架：新建即全层级 + 占位覆盖语义 + 旧项目补骨架幂等
// ---------------------------------------------------------------------------

/** 默认树占位文件全集（hub 相对路径——GET files 树应全部可见）。 */
const SKELETON_FILES: [&str; 13] = [
    "story/story.md",
    "story/_sources.md",
    "storyboard/storyboard.json",
    "casting/extraction.json",
    "casting/characters/_about.md",
    "casting/props/_about.md",
    "casting/pets/_about.md",
    "casting/formations/_about.md",
    "casting/actions/_about.md",
    "casting/scenes/_about.md",
    "audio/bgm/_about.md",
    "cache/_about.md",
    "dist/_about.md",
];

#[tokio::test]
async fn create_project_files_tree_contains_full_default_skeleton() {
    let (h, _dir) = handler_at("hub-skeleton");
    let (id, dir) = create_project(&h, "16:9").await;
    let root = format!("{dir}/hub");
    // GET files 树：占位文件全部可见（树扫描只认文件——空目录不可见，
    // 占位撑起 story/storyboard/casting 六类/audio/cache/dist 全层级）
    let resp = h
        .handle(get_req(&format!("/api/v1/film/projects/{id}/files")))
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    let paths: Vec<&str> = resp.body["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    for expect in SKELETON_FILES {
        assert!(paths.contains(&expect), "默认树缺 {expect}: {paths:?}");
    }
    for root_file in ["project.md", "README.md", "budget.json", "assets.json", "ownership.json", "activity.json"] {
        assert!(paths.contains(&root_file), "根元文件缺 {root_file}");
    }
    // 占位内容自文档：story.md front-matter（words=0 未创作标记）；extraction
    // 六类空数组（weapons 键对应 props/ 目录）；storyboard 空分镜骨架
    let story = std::fs::read_to_string(format!("{root}/story/story.md")).unwrap();
    let (sfm, sbody) = split_front_matter(&story);
    assert_eq!(sfm.get("words").unwrap(), "0");
    assert_eq!(sfm.get("summary").unwrap(), "待创作");
    assert!(sbody.contains("剧情页导入原文"), "{sbody}");
    let extraction = read_json(&format!("{root}/casting/extraction.json"));
    for key in [
        "characters",
        "weapons",
        "pets",
        "formations",
        "actions",
        "scenes",
    ] {
        assert!(
            extraction[key].as_array().is_some_and(Vec::is_empty),
            "extraction 占位 {key} 应为空数组: {extraction}"
        );
    }
    let sb = read_json(&format!("{root}/storyboard/storyboard.json"));
    assert_eq!(sb["version"], 1);
    assert!(sb["shots"].as_array().is_some_and(Vec::is_empty));
    // _about.md 可经 files 单文件读（自文档可浏览）
    let resp = h
        .handle(get_req(&format!(
            "/api/v1/film/projects/{id}/files/casting/characters/_about.md"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "{resp:?}");
    assert_eq!(resp.body["kind"], "text");
    assert!(
        resp.body["content"]
            .as_str()
            .unwrap()
            .contains("人物定妆"),
        "_about.md 说明文案"
    );
}

#[tokio::test]
async fn story_placeholder_is_not_treated_as_real_story() {
    let (h, _dir) = handler_at("hub-placeholder-guard");
    let (id, _dir2) = create_project(&h, "16:9").await;
    // 占位 story.md（words=0）不应被当成剧情正稿：casting/extract 应报
    // 「剧情正稿缺失」而不是拿占位文案去提取
    let (task, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/casting/extract"),
        serde_json::json!({"model_ref": {"source": "local", "capability": "chat"}}),
    )
    .await;
    assert_eq!(task["status"], "error", "{task:?}");
    assert!(
        task["error"]
            .as_str()
            .unwrap()
            .contains("剧情正稿缺失"),
        "占位不算真值: {task:?}"
    );
}

#[tokio::test]
async fn generation_overwrites_placeholders_without_residue() {
    let (mut h, _dir) = handler_at("hub-placeholder-overwrite");
    let content = "【第一幕】猫在灯塔下等待。\n【第二幕】雨停，回家。";
    let shots_json = serde_json::json!([
        {"shot":1,"desc":"灯塔下等待","image_prompt":"雨夜灯塔","video_prompt":"缓推","line":"","duration_secs":4},
    ])
    .to_string();
    let (port, _hits) = spawn_mock_upstream(vec![chat_response(content), chat_response(&shots_json)]);
    h = h.with_local_chat(port, "qwen-test");
    let (id, dir) = create_project(&h, "16:9").await;
    let root = format!("{dir}/hub");
    // story.generate 覆盖占位 story.md（无占位残留）
    let (task, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/story/generate"),
        serde_json::json!({"model_ref": {"source": "local", "capability": "chat"}}),
    )
    .await;
    assert_eq!(task["status"], "done", "{task:?}");
    let story = std::fs::read_to_string(format!("{root}/story/story.md")).unwrap();
    assert!(story.contains("【第一幕】"), "{story}");
    assert!(
        !story.contains("剧情页导入原文"),
        "占位正文应被整文件覆盖: {story}"
    );
    let (sfm, _) = split_front_matter(&story);
    assert_ne!(sfm.get("words").unwrap(), "0", "words 应为真实字数");
    // storyboard.generate 覆盖占位 storyboard.json（真分镜）
    let (task2, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/storyboard/generate"),
        serde_json::json!({"model_ref": {"source": "local", "capability": "chat"}}),
    )
    .await;
    assert_eq!(task2["status"], "done", "{task2:?}");
    let sb = read_json(&format!("{root}/storyboard/storyboard.json"));
    assert!(
        !sb["shots"].as_array().unwrap().is_empty(),
        "占位 shots 应被真分镜覆盖: {sb}"
    );
    // casting 建对象不删 _about；BGM 建轨不删 _about
    let resp = h
        .handle(post_req(
            &format!("/api/v1/film/projects/{id}/casting/characters"),
            serde_json::json!({"name": "小明", "desc": "黑发少年"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201, "{resp:?}");
    let resp = h
        .handle(post_req(
            &format!("/api/v1/film/projects/{id}/audio/bgm"),
            serde_json::json!({"name": "主题", "info": {"trigger": "global"}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201, "{resp:?}");
    assert!(
        Path::new(&format!("{root}/casting/characters/_about.md")).is_file(),
        "建对象不删 _about.md"
    );
    assert!(
        Path::new(&format!("{root}/audio/bgm/_about.md")).is_file(),
        "建音轨不删 _about.md"
    );
}

#[tokio::test]
async fn legacy_export_backfills_skeleton_and_is_idempotent() {
    let (h, dir) = handler_at("hub-skeleton-idempotent");
    // 手工造旧项目（无 hub/）：DB 行 + 根 script.json
    let proj_dir = dir.join("root").join("film-legacy2");
    std::fs::create_dir_all(&proj_dir).unwrap();
    {
        let conn = h.db.lock().unwrap();
        conn.execute(
            "INSERT INTO film_projects (id,title,idea,ratio,style_hint,status,dir,created_at,updated_at)
             VALUES ('film-legacy2','老片二','猫回家','16:9',NULL,'draft',?1,'2026','2026')",
            params![proj_dir.to_str().unwrap()],
        )
        .unwrap();
    }
    seed_script(proj_dir.to_str().unwrap(), vec![shot_json(1, "", 4)]);
    let root = format!("{}/hub", proj_dir.to_str().unwrap());
    // 首次惰性初始化：script.json 平移 storyboard.json 先落，占位后补不覆盖
    let resp = h
        .handle(get_req("/api/v1/film/projects/film-legacy2/files"))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "{resp:?}");
    let sb = read_json(&format!("{root}/storyboard/storyboard.json"));
    assert_eq!(sb["shots"].as_array().unwrap().len(), 1, "真值优先于占位");
    for f in SKELETON_FILES {
        assert!(Path::new(&format!("{root}/{f}")).is_file(), "补骨架缺 {f}");
    }
    // 再 export：已有文件原样保留（不重复造、内容不变）
    let before: Vec<(String, String)> = SKELETON_FILES
        .iter()
        .map(|f| {
            (
                f.to_string(),
                std::fs::read_to_string(format!("{root}/{f}")).unwrap(),
            )
        })
        .collect();
    let resp = h
        .handle(post_req("/api/v1/film/projects/film-legacy2/export", Value::Null))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "{resp:?}");
    for (f, content) in &before {
        assert_eq!(
            &std::fs::read_to_string(format!("{root}/{f}")).unwrap(),
            content,
            "export 幂等：{f} 不被改写"
        );
    }
    // 手写真值后再 export 也不被占位冲掉（仅缺失时写）
    std::fs::write(
        format!("{root}/story/story.md"),
        "---\nwords: 88\nsummary: 手写\n---\n手写剧情正文",
    )
    .unwrap();
    let resp = h
        .handle(post_req("/api/v1/film/projects/film-legacy2/export", Value::Null))
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    let story = std::fs::read_to_string(format!("{root}/story/story.md")).unwrap();
    assert!(story.contains("手写剧情正文"), "真值保留: {story}");
    assert!(!story.contains("剧情页导入原文"));
}

#[tokio::test]
async fn repeated_export_never_deletes_existing_files() {
    // 数据安全回归（2026-09-06 事故排查项）：export/ensure 幂等 = 只补缺不删除
    let (h, _base) = handler_at("export-zero-del");
    let (id, dir_p) = create_project(&h, "16:9").await;
    seed_real_data_files(&dir_p);
    let before = count_all_files(&format!("{dir_p}/hub"));
    assert!(before >= 3, "起步至少含种子文件: {before}");
    // 连续三次 export（每次 ensure_hub + export_inner）
    for _ in 0..3 {
        let resp = h
            .handle(post_req(
                &format!("/api/v1/film/projects/{id}/export"),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "{resp:?}");
    }
    let after = count_all_files(&format!("{dir_p}/hub"));
    assert!(
        after >= before,
        "export 不得删除既有文件（before={before}, after={after}）"
    );
    // 种子真值逐一仍在
    for f in ["story/story.md", "casting/characters/_about.md"] {
        assert!(Path::new(&format!("{dir_p}/hub/{f}")).is_file(), "{f} 应保留");
    }
    // 项目根产物（script.json/shot/角色图）不被 export 触碰
    for f in ["script.json", "shot-1.png", "characters/char-1/portrait.png"] {
        assert!(Path::new(&format!("{dir_p}/{f}")).is_file(), "{f} 应保留");
    }
}

/** 种子真实数据到项目根（数据安全回归用）。 */
fn seed_real_data_files(dir: &str) {
    std::fs::create_dir_all(format!("{dir}/characters/char-1")).unwrap();
    std::fs::write(
        format!("{dir}/script.json"),
        r#"{"shots":[{"shot":1,"desc":"d","image_prompt":"p","video_prompt":"v","line":"","duration_secs":5}],"generated_by":"seed","created_at":"2026"}"#,
    )
    .unwrap();
    std::fs::write(format!("{dir}/characters/char-1/portrait.png"), "png").unwrap();
    std::fs::write(format!("{dir}/shot-1.png"), "img").unwrap();
    std::fs::create_dir_all(format!("{dir}/hub/story")).unwrap();
    std::fs::write(format!("{dir}/hub/story/story.md"), "---\nwords: 88\n---\n剧情正文").unwrap();
}

/** 递归统计目录内文件数（含隐藏；不存在 0）。 */
fn count_all_files(dir: &str) -> usize {
    fn walk(p: &Path) -> usize {
        std::fs::read_dir(p)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| if e.path().is_dir() { walk(&e.path()) } else { 1 })
                    .sum()
            })
            .unwrap_or(0)
    }
    walk(Path::new(dir))
}

// ---------------------------------------------------------------------------
// story：导入 + AI 写剧情（author 流水断言 ①）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn story_import_validates_and_lists() {
    let (h, _dir) = handler_at("story-import");
    let (id, dir) = create_project(&h, "16:9").await;
    let path = format!("/api/v1/film/projects/{id}/story/import");
    // 正常导入（CJK 文件名 slug 化）
    let resp = h
        .handle(post_req(
            &path,
            serde_json::json!({
                "filename": "小说原文.txt",
                "content_b64": b64("很长的一条回家路，猫在霓虹里穿行……".as_bytes()),
                "author": "alice",
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201, "{resp:?}");
    assert_eq!(resp.body["path"], "story/source-小说原文.txt");
    let saved = std::fs::read_to_string(format!("{dir}/hub/story/source-小说原文.txt")).unwrap();
    assert!(saved.contains("回家路"));
    // activity 落 story.import
    let acts = activity_list(&dir);
    assert!(
        acts.iter()
            .any(|a| a["action"] == "story.import" && a["author"] == "alice"),
        "{acts:?}"
    );
    // 多份共存
    let resp = h
        .handle(post_req(
            &path,
            serde_json::json!({"filename": "second.md", "content_b64": b64("第二份原文".as_bytes())}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201);
    assert_eq!(
        resp.body["path"], "story/source-second.txt",
        "扩展名剥除后 slug 化"
    );
    // 校验矩阵：路径分隔符 / 坏 b64 / 超上限（env 缺省 64MB）/ 非 UTF-8
    let mut big = vec![b'x'; story_import_max_bytes().unwrap_or(64 * 1024 * 1024) + 1];
    big[0] = 0xE4; // 保持 UTF-8 无关——大小先拦
    for (body, mark) in [
        (
            serde_json::json!({"filename": "a/b.txt", "content_b64": b64(b"x")}),
            "路径分隔符",
        ),
        (
            serde_json::json!({"filename": "x.txt", "content_b64": "!!not-b64!!"}),
            "坏 b64",
        ),
        (
            serde_json::json!({"filename": "x.txt", "content_b64": b64(&big)}),
            "超 2MB",
        ),
        (
            serde_json::json!({"filename": "x.txt", "content_b64": b64(&[0xFF, 0xFE, 0x00])}),
            "非 UTF-8",
        ),
        (
            serde_json::json!({"filename": " ", "content_b64": b64(b"x")}),
            "空名",
        ),
    ] {
        let resp = h.handle(post_req(&path, body)).await.unwrap();
        assert_eq!(resp.status, 400, "{mark} 应 400: {resp:?}");
    }
}

#[tokio::test]
async fn story_generate_writes_story_md_and_readme_stage() {
    let (mut h, _dir) = handler_at("story-gen");
    let content = "【第一幕】霓虹雨夜，猫在巷口徘徊，远处灯塔亮起。\n小明提灯走来，唤它回家。\n【第二幕】屋顶追逐，雨停，猫入门。";
    let (port, hits) = spawn_mock_upstream(vec![chat_response(content)]);
    h = h.with_local_chat(port, "qwen-test");
    let (id, dir) = create_project(&h, "16:9").await;
    // 先导入原文，再基于原文改编
    h.handle(post_req(
        &format!("/api/v1/film/projects/{id}/story/import"),
        serde_json::json!({"filename": "原文.txt", "content_b64": b64("原著小说主线：猫、灯塔、回家。".as_bytes())}),
    ))
    .await
    .unwrap();
    let (task, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/story/generate"),
        serde_json::json!({
            "model_ref": {"source": "local", "capability": "chat"},
            "source_file": "原文.txt",
            "author": "alice",
        }),
    )
    .await;
    assert_eq!(task["status"], "done", "{task:?}");
    let story_path = format!("{dir}/hub/story/story.md");
    let raw = std::fs::read_to_string(&story_path).unwrap();
    assert!(raw.contains("【第一幕】") && raw.contains("灯塔"), "{raw}");
    let (fm, body) = split_front_matter(&raw);
    assert_eq!(fm.get("source").unwrap(), "source-原文.txt");
    assert!(
        fm.get("words").unwrap().parse::<usize>().unwrap() > 0,
        "字数入 front-matter"
    );
    assert_eq!(
        fm.get("summary").unwrap(),
        "小明提灯走来，唤它回家。",
        "梗概取首个非幕标题行"
    );
    assert!(body.contains("小明提灯"));
    // README 阶段推进
    let readme = std::fs::read_to_string(format!("{dir}/hub/README.md")).unwrap();
    let (rfm, _) = split_front_matter(&readme);
    assert_eq!(rfm.get("stage").unwrap(), "story");
    // 提示词：改编原文分支 + 题材硬约束 + 创意锚定
    let req0 = hits.lock().unwrap()[0].clone();
    assert!(
        req0.contains("【改编原文】") && req0.contains("原著小说主线"),
        "{req0}"
    );
    assert!(req0.contains("禁止更换题材"), "{req0}");
    assert!(req0.contains("一只猫在霓虹城市里寻找回家路"), "{req0}");
    // author 落流水（断言 ①：任务类端点）
    let acts = activity_list(&dir);
    assert!(
        acts.iter()
            .any(|a| a["action"] == "story.generate" && a["author"] == "alice"),
        "{acts:?}"
    );
    // source_file 不存在 → 任务 error 如实
    let (task2, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/story/generate"),
        serde_json::json!({"model_ref": {"source": "local", "capability": "chat"}, "source_file": "不存在.txt"}),
    )
    .await;
    assert_eq!(task2["status"], "error");
    assert!(task2["error"].as_str().unwrap().contains("导入原文不存在"));
}

// ---------------------------------------------------------------------------
// storyboard：读剧情生成分镜 + script 兼容别名
// ---------------------------------------------------------------------------

#[tokio::test]
async fn storyboard_generate_reads_story_and_fills_casting_slots() {
    let (mut h, _dir) = handler_at("sb-gen");
    let content = serde_json::json!([
        {"shot":1,"desc":"灯塔下猫徘徊","image_prompt":"雨夜灯塔关键帧","video_prompt":"缓慢推进","line":"这是哪里？","duration_secs":5,
         "characters":["小明"],"props":["长刀"],"pets":["黑猫"],"scenes":["灯塔顶"],"actions":["拔剑"]},
        {"shot":2,"desc":"屋顶追逐","image_prompt":"追逐关键帧","video_prompt":"横移","line":"","duration_secs":4},
    ])
    .to_string();
    let (port, hits) = spawn_mock_upstream(vec![chat_response(&content)]);
    h = h.with_local_chat(port, "qwen-test");
    let (id, dir) = create_project(&h, "16:9").await;
    seed_story_md(
        &dir,
        "【第一幕】灯塔下的猫，小明提长刀而来。\n【第二幕】黑猫跃上屋顶追逐。",
    );
    let (task, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/storyboard/generate"),
        serde_json::json!({"model_ref": {"source": "local", "capability": "chat"}, "author": "bob"}),
    )
    .await;
    assert_eq!(task["status"], "done", "{task:?}");
    assert!(
        task["output"]
            .as_str()
            .unwrap()
            .ends_with("storyboard.json"),
        "新端点 output=树真值"
    );
    // storyboard.json（树真值）+ script.json（画布镜像）双写
    let sb = read_json(&format!("{dir}/hub/storyboard/storyboard.json"));
    let shots = sb["shots"].as_array().unwrap();
    assert_eq!(shots.len(), 2);
    assert_eq!(shots[0]["characters"], serde_json::json!(["小明"]));
    assert_eq!(shots[0]["props"], serde_json::json!(["长刀"]));
    assert_eq!(shots[0]["pets"], serde_json::json!(["黑猫"]));
    assert_eq!(shots[0]["scenes"], serde_json::json!(["灯塔顶"]));
    assert_eq!(shots[0]["actions"], serde_json::json!(["拔剑"]));
    let sc = read_json(&format!("{dir}/script.json"));
    assert_eq!(
        sc["shots"][0]["props"],
        serde_json::json!(["长刀"]),
        "镜像零翻译"
    );
    // README 阶段推进 + 状态 scripted
    let readme = std::fs::read_to_string(format!("{dir}/hub/README.md")).unwrap();
    let (rfm, _) = split_front_matter(&readme);
    assert_eq!(rfm.get("stage").unwrap(), "storyboard");
    let resp = h
        .handle(get_req(&format!("/api/v1/film/projects/{id}")))
        .await
        .unwrap();
    assert_eq!(resp.body["project"]["status"], "scripted");
    // 提示词：读剧情 + casting 字段说明 + 题材硬约束
    let req0 = hits.lock().unwrap()[0].clone();
    assert!(req0.contains("灯塔下的猫"), "story 正文入 prompt: {req0}");
    assert!(req0.contains("从剧情逐幕分析"), "{req0}");
    assert!(req0.contains("props"), "{req0}");
    assert!(req0.contains("禁止更换题材"), "{req0}");
    // activity
    let acts = activity_list(&dir);
    assert!(
        acts.iter()
            .any(|a| a["action"] == "storyboard.generate" && a["author"] == "bob"),
        "{acts:?}"
    );
}

#[tokio::test]
async fn script_alias_reuses_storyboard_chain_with_legacy_output() {
    let (mut h, _dir) = handler_at("sb-alias");
    let content = serde_json::json!([
        {"shot":1,"desc":"开场","image_prompt":"p","video_prompt":"v","line":"hi","duration_secs":5}
    ])
    .to_string();
    let (port, _hits) = spawn_mock_upstream(vec![chat_response(&content)]);
    h = h.with_local_chat(port, "qwen-test");
    let (id, dir) = create_project(&h, "16:9").await;
    let (task, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/script"),
        serde_json::json!({"model_ref": {"source": "local", "capability": "chat"}}),
    )
    .await;
    assert_eq!(task["status"], "done", "{task:?}");
    // 兼容契约：output 指 script.json（旧前端），同时树内 storyboard.json 双写
    assert!(task["output"].as_str().unwrap().ends_with("script.json"));
    assert!(Path::new(&format!("{dir}/hub/storyboard/storyboard.json")).is_file());
    // 无 story.md 时回落【创意】
    let resp = h
        .handle(get_req(&format!("/api/v1/film/projects/{id}")))
        .await
        .unwrap();
    assert_eq!(resp.body["script"].as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// casting：提取端点 + CRUD + 对象级认领 + 视图双源
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extract_requires_story_and_writes_report_when_ready() {
    let (mut h, _dir) = handler_at("extract2");
    let report = r#"{"characters":[{"name":"小明","desc":"黑发少年","frequency":2,"reason":"主角"}],"weapons":[{"name":"长刀","desc":"佩刀","frequency":1,"reason":"高频道具"}],"pets":[{"name":"黑猫","frequency":2,"reason":"贯穿"}],"formations":[],"actions":[{"name":"拔剑","frequency":1,"reason":""}],"scenes":[{"name":"灯塔顶","frequency":2,"reason":""}]}"#;
    let (port, hits) = spawn_mock_upstream(vec![chat_response(&format!(
        "好的。\n```json\n{report}\n```"
    ))]);
    h = h.with_local_chat(port, "qwen-test");
    let (id, dir) = create_project(&h, "16:9").await;
    // ① 缺剧情 → 任务 error 如实
    let (t0, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/casting/extract"),
        serde_json::json!({"model_ref": {"source": "local", "capability": "chat"}}),
    )
    .await;
    assert_eq!(t0["status"], "error", "{t0:?}");
    assert!(t0["error"].as_str().unwrap().contains("story"));
    // ② 齐备 → extraction.json（六类，weapons 键按冻结契约保留）+ README casting
    seed_story_md(
        &dir,
        "【第一幕】小明持长刀在灯塔顶，黑猫徘徊。\n【第二幕】拔剑，追逐。",
    );
    seed_script(&dir, vec![shot_json(1, "", 5), shot_json(2, "", 4)]);
    let (task, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/casting/extract"),
        serde_json::json!({"model_ref": {"source": "local", "capability": "chat"}, "author": "carol"}),
    )
    .await;
    assert_eq!(task["status"], "done", "{task:?}");
    let ext = read_json(&format!("{dir}/hub/casting/extraction.json"));
    assert_eq!(ext["characters"][0]["name"], "小明");
    assert_eq!(
        ext["weapons"][0]["name"], "长刀",
        "weapons 键（对应 casting/props/ 目录）"
    );
    assert_eq!(ext["pets"][0]["frequency"], 2);
    assert_eq!(ext["formations"], serde_json::json!([]));
    let readme = std::fs::read_to_string(format!("{dir}/hub/README.md")).unwrap();
    let (rfm, _) = split_front_matter(&readme);
    assert_eq!(rfm.get("stage").unwrap(), "casting");
    // 提示词含六类定义 + story 与分镜内容
    let req0 = hits.lock().unwrap()[0].clone();
    assert!(req0.contains("定妆统筹") || req0.contains("六类"), "{req0}");
    assert!(req0.contains("灯塔顶"), "{req0}");
    let acts = activity_list(&dir);
    assert!(
        acts.iter().any(|a| a["action"] == "casting.extract"),
        "{acts:?}"
    );
}

#[tokio::test]
async fn casting_crud_slug_conflict_rename_and_auto_claim() {
    let (h, _dir) = handler_at("cast-crud");
    let (id, dir) = create_project(&h, "16:9").await;
    let base = format!("/api/v1/film/projects/{id}/casting");
    // 未知类别 404
    let resp = h.handle(get_req(&format!("{base}/weapons"))).await.unwrap();
    assert_eq!(
        resp.status, 404,
        "六类枚举外（weapons 对应 props/）: {resp:?}"
    );
    // 建（带 author → 对象级自动认领）
    let resp = h
        .handle(post_req(
            &format!("{base}/characters"),
            serde_json::json!({"name": "小明", "desc": "黑发少年，红围巾", "voice": "onyx", "author": "alice"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201, "{resp:?}");
    assert_eq!(resp.body["slug"], "小明");
    assert_eq!(resp.body["claimed_by"], "alice", "自动认领 owner=author");
    let card_p = format!("{dir}/hub/casting/characters/小明/card.md");
    let (fm, body) = split_front_matter(&std::fs::read_to_string(&card_p).unwrap());
    assert_eq!(fm.get("name").unwrap(), "小明");
    assert_eq!(fm.get("voice").unwrap(), "onyx");
    assert!(body.contains("黑发少年"));
    // ownership.json 对象级认领表
    let own = read_json(&format!("{dir}/hub/ownership.json"));
    assert_eq!(own["casting_objects"]["characters/小明"]["owner"], "alice");
    // activity：casting.create + casting.claim
    let acts = activity_list(&dir);
    assert!(
        acts.iter()
            .any(|a| a["action"] == "casting.claim" && a["author"] == "alice"),
        "{acts:?}"
    );
    assert!(
        acts.iter().any(|a| a["action"] == "casting.create"),
        "{acts:?}"
    );
    // 重名 409（slug 与卡名双重判定）
    for body_json in [
        serde_json::json!({"name": "小明", "desc": "另一个"}),
        serde_json::json!({"name": " 小明 ", "desc": "空白差异同名"}),
    ] {
        let resp = h
            .handle(post_req(&format!("{base}/characters"), body_json))
            .await
            .unwrap();
        assert_eq!(resp.status, 409, "{resp:?}");
    }
    // 空字段 400
    for bad in [
        serde_json::json!({"name": " ", "desc": "d"}),
        serde_json::json!({"name": "n", "desc": " "}),
    ] {
        let resp = h
            .handle(post_req(&format!("{base}/props"), bad))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "{resp:?}");
    }
    // GET 列表
    let resp = h
        .handle(get_req(&format!("{base}/characters")))
        .await
        .unwrap();
    let list = resp.body.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["claimed_by"], "alice");
    // PUT 改名：目录迁移 + 认领键迁移
    let resp = h
        .handle(put_req(
            &format!("{base}/characters/小明"),
            serde_json::json!({"name": "小明二", "desc": "黑发少年，蓝围巾"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "{resp:?}");
    assert_eq!(resp.body["slug"], "小明二");
    assert!(Path::new(&format!("{dir}/hub/casting/characters/小明二/card.md")).is_file());
    assert!(!Path::new(&card_p).exists(), "旧目录已迁移");
    let own2 = read_json(&format!("{dir}/hub/ownership.json"));
    assert!(
        own2["casting_objects"].get("characters/小明").is_none(),
        "旧认领键迁移"
    );
    assert_eq!(
        own2["casting_objects"]["characters/小明二"]["owner"],
        "alice"
    );
    // 404 矩阵
    let resp = h
        .handle(put_req(
            &format!("{base}/characters/路人"),
            serde_json::json!({"desc": "x"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 404);
    // DELETE：目录 + 认领
    let resp = h
        .handle(delete_req(&format!("{base}/characters/小明二")))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "{resp:?}");
    assert!(!Path::new(&format!("{dir}/hub/casting/characters/小明二")).exists());
    let own3 = read_json(&format!("{dir}/hub/ownership.json"));
    assert!(own3["casting_objects"].as_object().unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn casting_view_generate_and_import_dual_source() {
    let (mut h, _dir) = handler_at("cast-view");
    let fixture = temp_dir_for("cast-view-fix");
    let smi = fake_exec(&fixture, "fake-smi.sh", "#!/bin/sh\necho 24000\n");
    let imggen = fake_exec(
        &fixture,
        "fake-imggen.sh",
        "#!/bin/sh\nprintf '\\211PNG\\015\\012\\032\\012view' > \"$NEXOS_IMGGEN_OUT\"\n",
    );
    h = h.with_imggen_mock(
        imggen.to_str().unwrap(),
        fixture.join("fake-imggen.sh").to_str().unwrap(),
        smi.to_str().unwrap(),
    );
    let (id, dir) = create_project(&h, "16:9").await;
    let base = format!("/api/v1/film/projects/{id}/casting");
    h.handle(post_req(
        &format!("{base}/characters"),
        serde_json::json!({"name": "小明", "desc": "黑发少年，红围巾"}),
    ))
    .await
    .unwrap();
    // view 名校验
    for bad in ["../evil", "a b", ""] {
        let resp = h
            .handle(post_req(
                &format!("{base}/characters/小明/views/generate"),
                serde_json::json!({"model_ref": {"source": "local", "capability": "image"}, "view": bad}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "view={bad}: {resp:?}");
    }
    // AI 生成（fake 内核）→ views/front.png + assets source=ai + portrait 回填
    let (task, _) = run_stage(
        &h,
        &format!("{base}/characters/小明/views/generate"),
        serde_json::json!({"model_ref": {"source": "local", "capability": "image"}, "view": "front", "author": "alice"}),
    )
    .await;
    assert_eq!(task["status"], "done", "{task:?}");
    let vpath = format!("{dir}/hub/casting/characters/小明/views/front.png");
    assert_eq!(std::fs::read(&vpath).unwrap(), png_bytes(b"view"));
    let assets = read_json(&format!("{dir}/hub/assets.json"));
    let entry = assets
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["path"] == "casting/characters/小明/views/front.png")
        .expect("资产登记");
    assert_eq!(entry["source"], "ai");
    assert_eq!(entry["ref"], "characters/小明");
    assert!(entry["sha256"].as_str().unwrap().len() == 64, "sha256 登记");
    let card =
        std::fs::read_to_string(format!("{dir}/hub/casting/characters/小明/card.md")).unwrap();
    let (fm, _) = split_front_matter(&card);
    assert_eq!(fm.get("portrait").unwrap(), "views/front.png", "主视图回填");
    // 导入参考图（side 视图）→ source=import
    let resp = h
        .handle(post_req(
            &format!("{base}/characters/小明/views/import"),
            serde_json::json!({"image_b64": b64(&png_bytes(b"side-import")), "view": "side", "author": "bob"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201, "{resp:?}");
    assert_eq!(resp.body["source"], "import");
    // mime 与魔数不符 400
    let resp = h
        .handle(post_req(
            &format!("{base}/characters/小明/views/import"),
            serde_json::json!({"image_b64": b64(&png_bytes(b"x")), "view": "back", "mime": "image/jpeg"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 400, "{resp:?}");
    // GET 列表两个视图
    let resp = h
        .handle(get_req(&format!("{base}/characters")))
        .await
        .unwrap();
    let views = resp.body[0]["views"].as_array().unwrap();
    assert_eq!(views.len(), 2);
    assert!(views
        .iter()
        .any(|v| v["view"] == "front" && v["url"].as_str().unwrap().contains("files/download")));
    // 对象不存在 404
    let resp = h
        .handle(post_req(
            &format!("{base}/characters/路人/views/import"),
            serde_json::json!({"image_b64": b64(&png_bytes(b"x")), "view": "front"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 404);
}

// ---------------------------------------------------------------------------
// BGM：建条目 / 导入（author 流水断言 ②）/ 删除 / 生成 / compose 选择
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bgm_crud_import_generate_and_trigger_validation() {
    let (h, _dir) = handler_at("bgm");
    let mp3 = b"ID3-fake-bgm-track".to_vec();
    let (port, hits) = spawn_mock_upstream(vec![serde_json::json!({"b64": b64(&mp3)})
        .to_string()
        .into_bytes()]);
    let gw = Arc::new(super::super::api_gateway::ApiGatewayRouteHandler::with_empty());
    let ch_id = seed_channel(&gw, &format!("http://127.0.0.1:{port}/v1")).await;
    let h = h.with_gateway(gw);
    let (id, dir) = create_project(&h, "16:9").await;
    let base = format!("/api/v1/film/projects/{id}/audio/bgm");
    // 先建条目（省略 track）
    let resp = h
        .handle(post_req(
            &base,
            serde_json::json!({"name": "theme", "info": {"trigger": "global", "mood": "恢弘"}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201, "{resp:?}");
    assert_eq!(resp.body["track"], "theme");
    assert_eq!(resp.body["trigger"], "global");
    assert_eq!(resp.body["bytes"], Value::Null, "无音频字节");
    // 导入（trigger=scene:灯塔顶；author bob → 断言 ②：同步导入类端点）
    let resp = h
        .handle(post_req(
            &base,
            serde_json::json!({
                "name": "scene1",
                "info": {"trigger": "scene:灯塔顶", "mood": "紧张", "duration": 120},
                "track_b64": b64(&mp3),
                "author": "bob",
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201, "{resp:?}");
    assert_eq!(resp.body["bytes"], mp3.len() as i64);
    assert!(
        Path::new(&format!("{dir}/hub/audio/bgm/scene1/track.mp3")).is_file(),
        "音频落 track.mp3"
    );
    let info = std::fs::read_to_string(format!("{dir}/hub/audio/bgm/scene1/info.md")).unwrap();
    let (fm, _) = split_front_matter(&info);
    assert_eq!(fm.get("trigger").unwrap(), "scene:灯塔顶");
    assert_eq!(fm.get("mood").unwrap(), "紧张");
    assert_eq!(fm.get("duration").unwrap(), "120");
    let acts = activity_list(&dir);
    assert!(
        acts.iter()
            .any(|a| a["action"] == "bgm.import" && a["author"] == "bob"),
        "{acts:?}"
    );
    // assets 登记（source=import）
    let assets = read_json(&format!("{dir}/hub/assets.json"));
    assert!(
        assets
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["path"] == "audio/bgm/scene1/track.mp3" && a["source"] == "import"),
        "{assets:?}"
    );
    // trigger 校验
    let resp = h
        .handle(post_req(
            &base,
            serde_json::json!({"name": "bad", "info": {"trigger": "always"}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 400, "{resp:?}");
    // 重名 409
    let resp = h
        .handle(post_req(&base, serde_json::json!({"name": "theme"})))
        .await
        .unwrap();
    assert_eq!(resp.status, 409, "{resp:?}");
    // 列表
    let resp = h.handle(get_req(&base)).await.unwrap();
    let tracks = resp.body["tracks"].as_array().unwrap();
    assert_eq!(tracks.len(), 2);
    assert!(tracks
        .iter()
        .any(|t| t["trigger"] == "scene:灯塔顶" && t["has_track"] == true));
    // AI 生成（渠道 mock；trigger 从 info.md 读进日志）
    let (task, _) = run_stage(
        &h,
        &format!("{base}/scene1/generate"),
        serde_json::json!({"model_ref": {"source": "channel", "channel_id": ch_id, "capability": "music"}}),
    )
    .await;
    assert_eq!(task["status"], "done", "{task:?}");
    assert_eq!(
        std::fs::read(format!("{dir}/hub/audio/bgm/scene1/track.mp3")).unwrap(),
        mp3
    );
    let req0 = hits.lock().unwrap()[0].clone();
    assert!(req0.contains("/v1/music/generations"), "{}", req0);
    assert!(
        req0.contains("紧张") && req0.contains("灯塔顶"),
        "缺省 prompt 读 info.md: {req0}"
    );
    let readme = std::fs::read_to_string(format!("{dir}/hub/README.md")).unwrap();
    let (rfm, _) = split_front_matter(&readme);
    assert_eq!(rfm.get("stage").unwrap(), "audio");
    // 未知音轨生成 404 / 删除
    let resp = h
        .handle(post_req(
            &format!("{base}/nope/generate"),
            serde_json::json!({"model_ref": {"source": "channel", "channel_id": ch_id, "capability": "music"}}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 404);
    let resp = h
        .handle(delete_req(&format!("{base}/theme")))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "{resp:?}");
    assert!(!Path::new(&format!("{dir}/hub/audio/bgm/theme")).exists());
}

#[cfg(unix)]
#[tokio::test]
async fn compose_selects_bgm_and_versions_dist() {
    let (mut h, _dir) = handler_at("compose-hub");
    let fixture = temp_dir_for("compose-hub-bin");
    let argv_log = fixture.join("argv.log");
    let ff = fake_exec(
        &fixture,
        "ffmpeg",
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nout=\"\"\nfor a in \"$@\"; do out=\"$a\"; done\n: > \"$out\"\nexit 0\n",
            argv_log.to_str().unwrap()
        ),
    );
    h = h.with_ffmpeg_bin(ff.to_str().unwrap());
    let (id, dir) = create_project(&h, "16:9").await;
    seed_script(&dir, vec![shot_json(1, "台词", 5)]);
    std::fs::write(format!("{dir}/shot-1.mp4"), b"mp4-1").unwrap();
    let mp3 = b"ID3-bgm".to_vec();
    let root = format!("{dir}/hub");
    for (name, trigger) in [("scene-track", "scene:灯塔顶"), ("global-track", "global")] {
        let d = format!("{root}/audio/bgm/{name}");
        std::fs::create_dir_all(&d).unwrap();
        write_bgm_info(&d, trigger, None, None);
        std::fs::write(format!("{d}/track.mp3"), &mp3).unwrap();
    }
    // ① 缺省：trigger=global 优先
    let (task, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/compose"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(task["status"], "done", "{task:?}");
    let raw = std::fs::read_to_string(&argv_log).unwrap();
    let pass2 = raw.lines().last().unwrap();
    assert!(
        pass2.contains("-stream_loop -1 -i hub/audio/bgm/global-track/track.mp3"),
        "global 优先缺省: {pass2}"
    );
    let out1 = task["output"].as_str().unwrap();
    assert!(
        out1.contains("hub/dist/final-v") && out1.ends_with(".mp4"),
        "版本化 dist: {out1}"
    );
    let report = read_json(&format!("{root}/dist/compose-report.json"));
    assert_eq!(report["bgm"]["track"], "global-track");
    assert_eq!(report["shots"], 1);
    assert_eq!(report["voices"], 0);
    // ② body 指定 scene 音轨
    let (task2, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/compose"),
        serde_json::json!({"bgm": "scene-track", "author": "alice"}),
    )
    .await;
    assert_eq!(task2["status"], "done", "{task2:?}");
    let raw2 = std::fs::read_to_string(&argv_log).unwrap();
    let pass2b = raw2.lines().last().unwrap();
    assert!(
        pass2b.contains("hub/audio/bgm/scene-track/track.mp3"),
        "指定音轨: {pass2b}"
    );
    // 指定不存在音轨 → 请求期 404
    let resp = h
        .handle(post_req(
            &format!("/api/v1/film/projects/{id}/compose"),
            serde_json::json!({"bgm": "nope"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 404, "{resp:?}");
    // 两次版本化成品共存
    let finals: Vec<String> = std::fs::read_dir(format!("{root}/dist"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("final-v"))
        .collect();
    assert_eq!(finals.len(), 2, "版本化共存不覆盖: {finals:?}");
    // README compose + activity
    let readme = std::fs::read_to_string(format!("{root}/README.md")).unwrap();
    let (rfm, _) = split_front_matter(&readme);
    assert_eq!(rfm.get("stage").unwrap(), "compose");
    let acts = activity_list(&dir);
    assert!(acts.iter().any(|a| a["action"] == "compose"), "{acts:?}");
}

// ---------------------------------------------------------------------------
// cache commit：半成品转正（author 流水断言 ③）
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn cache_commit_promotes_trial_artifacts() {
    let (mut h, _dir) = handler_at("cache-commit");
    let fixture = temp_dir_for("cache-commit-fix");
    let smi = fake_exec(&fixture, "fake-smi.sh", "#!/bin/sh\necho 24000\n");
    let imggen = fake_exec(
        &fixture,
        "fake-imggen.sh",
        "#!/bin/sh\nprintf '\\211PNG\\015\\012\\032\\012film' > \"$NEXOS_IMGGEN_OUT\"\n",
    );
    h = h.with_imggen_mock(
        imggen.to_str().unwrap(),
        fixture.join("fake-imggen.sh").to_str().unwrap(),
        smi.to_str().unwrap(),
    );
    let (id, dir) = create_project(&h, "16:9").await;
    seed_script(&dir, vec![shot_json(1, "", 5)]);
    // 试生成落 cache
    let (task, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/shots/1/image"),
        serde_json::json!({"model_ref": {"source": "local", "capability": "image"}, "author": "carol"}),
    )
    .await;
    assert_eq!(task["status"], "done", "{task:?}");
    let cache_file = format!("{dir}/hub/cache/shot-1.png");
    assert!(Path::new(&cache_file).is_file(), "半成品在 cache");
    assert!(
        !Path::new(&format!("{dir}/shot-1.png")).is_file(),
        "正式位未占用"
    );
    // compose 被缺视频拦（cache 未转正不认）+ 提示 commit
    let (t_err, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/compose"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(t_err["status"], "error");
    assert!(
        t_err["error"]
            .as_str()
            .unwrap()
            .contains("cache/shot-1.mp4/commit")
            || t_err["error"]
                .as_str()
                .unwrap()
                .contains("先完成各镜头 video 阶段"),
        "{}",
        t_err["error"].as_str().unwrap()
    );
    // 名字白名单
    for (bad, want) in [
        ("shot-x.png", 400),
        ("line-1.png", 400),
        ("bgm.mp3", 400),
        ("notes.txt", 400),
        ("../evil.png", 404), // 含斜杠多一段 → 路由不匹配（同为拒绝面）
    ] {
        let resp = h
            .handle(post_req(
                &format!("/api/v1/film/projects/{id}/cache/{bad}/commit"),
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, want, "{bad}: {resp:?}");
    }
    // 转正（author carol → 断言 ③）
    let resp = h
        .handle(post_req(
            &format!("/api/v1/film/projects/{id}/cache/shot-1.png/commit"),
            serde_json::json!({"author": "carol"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "{resp:?}");
    assert_eq!(resp.body["committed"], "shot-1.png");
    assert!(
        Path::new(&format!("{dir}/shot-1.png")).is_file(),
        "正式位就绪"
    );
    assert!(!Path::new(&cache_file).exists(), "cache 清空");
    let acts = activity_list(&dir);
    assert!(
        acts.iter().any(|a| a["action"] == "cache.commit"
            && a["author"] == "carol"
            && a["target"] == "shot-1.png"),
        "{acts:?}"
    );
    // 试生成流水也有 author
    assert!(
        acts.iter()
            .any(|a| a["action"] == "shot.image" && a["author"] == "carol"),
        "{acts:?}"
    );
    // 重复 commit → 404（cache 已空）
    let resp = h
        .handle(post_req(
            &format!("/api/v1/film/projects/{id}/cache/shot-1.png/commit"),
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 404, "{resp:?}");
}

// ---------------------------------------------------------------------------
// files 面：读 / 写 / 穿越 / ownership PUT 校验 / import 应用
// ---------------------------------------------------------------------------

#[tokio::test]
async fn files_get_put_whitelist_and_traversal() {
    let (h, _dir) = handler_at("files");
    let (id, dir) = create_project(&h, "16:9").await;
    let root = format!("{dir}/hub");
    // 文本读
    let resp = h
        .handle(get_req(&format!(
            "/api/v1/film/projects/{id}/files/project.md"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "{resp:?}");
    assert_eq!(resp.body["kind"], "text");
    assert!(resp.body["content"]
        .as_str()
        .unwrap()
        .contains("一只猫在霓虹城市里寻找回家路"));
    // 二进制读（先落一张视图）
    std::fs::create_dir_all(format!("{root}/casting/characters/小明/views")).unwrap();
    std::fs::write(
        format!("{root}/casting/characters/小明/views/front.png"),
        png_bytes(b"v"),
    )
    .unwrap();
    let resp = h
        .handle(get_req(&format!(
            "/api/v1/film/projects/{id}/files/casting/characters/{}/views/front.png",
            percent_decode("%E5%B0%8F%E6%98%8E")
        )))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "CJK 路径段百分号解码: {resp:?}");
    assert_eq!(resp.body["kind"], "binary");
    assert_eq!(resp.body["mime"], "image/png");
    assert_eq!(resp.body["bytes"], png_bytes(b"v").len() as i64);
    // 白名单外读（cache 二进制允许；根外文件不可达——路径穿越拦截）
    let resp = h
        .handle(get_req(&format!(
            "/api/v1/film/projects/{id}/files/../../../etc/passwd"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status, 400, "穿越段拦截: {resp:?}");
    let resp = h
        .handle(get_req(&format!(
            "/api/v1/film/projects/{id}/files/story/..%2F..%2Ffilm.db"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status, 400, "编码穿越拦截: {resp:?}");
    let resp = h
        .handle(get_req(&format!(
            "/api/v1/film/projects/{id}/files/nope.md"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status, 404);
    // PUT：storyboard.json 合法 JSON 过 + 应用走 import
    seed_script(&dir, vec![shot_json(1, "旧", 5)]);
    let sb = serde_json::json!({
        "version": 1,
        "shots": [
            {"shot":1,"desc":"新画面","image_prompt":"np","video_prompt":"nv","line":"新台词","duration_secs":3,
             "characters":["小明"],"props":["长刀"],"pets":[],"scenes":[],"actions":[]}
        ],
    });
    let resp = h
        .handle(put_req(
            &format!("/api/v1/film/projects/{id}/files/storyboard/storyboard.json"),
            serde_json::json!({"content": serde_json::to_string_pretty(&sb).unwrap(), "author": "agent"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "{resp:?}");
    // 手改不自动应用：script.json 仍是旧
    assert!(std::fs::read_to_string(format!("{dir}/script.json"))
        .unwrap()
        .contains("镜头1画面"));
    // import 应用 + 未知 casting 引用报告
    let resp = h
        .handle(post_req(
            &format!("/api/v1/film/projects/{id}/import"),
            serde_json::json!({"author": "agent"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "{resp:?}");
    assert_eq!(resp.body["applied"]["shots"], 1);
    let unknown = resp.body["unknown_casting_refs"].as_array().unwrap();
    assert!(
        unknown.contains(&serde_json::json!("小明"))
            && unknown.contains(&serde_json::json!("长刀")),
        "{unknown:?}"
    );
    let sc = read_json(&format!("{dir}/script.json"));
    assert_eq!(sc["shots"][0]["line"], "新台词", "import 应用到画布");
    // PUT 拒绝面：assets.json 服务端真值 / 非法 JSON storyboard / 二进制
    for (path, body, mark) in [
        (
            "assets.json",
            serde_json::json!({"content": "[]"}),
            "服务端真值",
        ),
        (
            "storyboard/storyboard.json",
            serde_json::json!({"content": "{not-json"}),
            "非法 JSON",
        ),
        (
            "casting/characters/小明/views/front.png",
            serde_json::json!({"content": "x"}),
            "二进制",
        ),
        (
            "../../etc/passwd",
            serde_json::json!({"content": "x"}),
            "穿越",
        ),
    ] {
        let resp = h
            .handle(put_req(
                &format!("/api/v1/film/projects/{id}/files/{path}"),
                body,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "{mark}: {resp:?}");
    }
    // budget.json 仅 budget_limit 生效（events 手写无效，重建自 DB）
    let resp = h
        .handle(put_req(
            &format!("/api/v1/film/projects/{id}/files/budget.json"),
            serde_json::json!({"content": "{\"budget_limit\": 50, \"events\": [{\"fake\": true}]}", "author": "a"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "{resp:?}");
    let budget = read_json(&format!("{root}/budget.json"));
    assert_eq!(budget["budget_limit"], 50.0);
    assert!(
        budget["events"].as_array().unwrap().is_empty(),
        "手写 events 无效（DB 真值）: {budget}"
    );
}

#[tokio::test]
async fn ownership_put_via_files_with_validation() {
    let (h, _dir) = handler_at("own-put");
    let (id, dir) = create_project(&h, "16:9").await;
    let root = format!("{dir}/hub");
    // 合法：分区认领 + 对象级认领（对象存在性宽容）
    let own = serde_json::json!({
        "members": [
            {"name": "alice", "joined_at": "2026-09-06T10:00:00+08:00"},
            {"name": "bob", "joined_at": "2026-09-06T11:00:00+08:00"},
        ],
        "sections": {
            "story": {"owner": "alice", "claimed_at": "2026-09-06T10:05:00+08:00"},
            "casting": {"owner": "bob", "claimed_at": "2026-09-06T11:05:00+08:00"},
        },
        "casting_objects": {
            "characters/小明": {"owner": "alice", "claimed_at": "2026-09-06T10:10:00+08:00"},
            "scenes/灯塔顶": {"owner": "bob", "claimed_at": "2026-09-06T11:10:00+08:00"},
        },
    });
    let resp = h
        .handle(put_req(
            &format!("/api/v1/film/projects/{id}/files/ownership.json"),
            serde_json::json!({"content": serde_json::to_string_pretty(&own).unwrap()}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "{resp:?}");
    let saved = read_json(&format!("{root}/ownership.json"));
    assert_eq!(saved["sections"]["story"]["owner"], "alice");
    assert_eq!(
        saved["casting_objects"]["characters/小明"]["owner"],
        "alice"
    );
    // GET 回读
    let resp = h
        .handle(get_req(&format!(
            "/api/v1/film/projects/{id}/files/ownership.json"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    // 拒绝面：sections 枚举 / casting_objects 键格式 / 非 JSON
    for (content, mark) in [
        (
            serde_json::json!({"sections": {"sound": {"owner": "a"}}}).to_string(),
            "sections 枚举",
        ),
        (
            serde_json::json!({"casting_objects": {"vehicles/车": {"owner": "a"}}}).to_string(),
            "type 枚举",
        ),
        (
            serde_json::json!({"casting_objects": {"characters/小明/extra": {"owner": "a"}}})
                .to_string(),
            "键多段",
        ),
        ("not-json".to_string(), "非 JSON"),
    ] {
        let resp = h
            .handle(put_req(
                &format!("/api/v1/film/projects/{id}/files/ownership.json"),
                serde_json::json!({"content": content}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "{mark}: {resp:?}");
    }
    // 失败不落盘
    let saved2 = read_json(&format!("{root}/ownership.json"));
    assert_eq!(saved2["sections"]["story"]["owner"], "alice");
}

// ---------------------------------------------------------------------------
// 成本记账：六阶段埋点 + 渠道单价 + 聚合
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn cost_accounting_six_stages_channel_prices_and_aggregation() {
    let (h, _dir) = handler_at("cost");
    // 渠道 mock：video + tts + music 三响应
    let mp4 = b"\x00\x00\x00\x18ftypmp4-hub".to_vec();
    let (dl_port, _dh) = spawn_mock_upstream(vec![mp4.clone()]);
    let (port, _hits) = spawn_mock_upstream(vec![
        serde_json::json!({"url": format!("http://127.0.0.1:{dl_port}/v.mp4")})
            .to_string()
            .into_bytes(),
        b"ID3-tts".to_vec(),
        serde_json::json!({"b64": b64(b"ID3-music")})
            .to_string()
            .into_bytes(),
    ]);
    let gw = Arc::new(super::super::api_gateway::ApiGatewayRouteHandler::with_empty());
    let ch_id = seed_channel(&gw, &format!("http://127.0.0.1:{port}/v1")).await;
    // 配置渠道单价（per_call=1.5，其余 0）
    let resp = gw
        .handle(put_req(
            &format!("/api/v1/gateway/channels/{ch_id}"),
            serde_json::json!({"price_per_call": 1.5}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "{resp:?}");
    let mut h = h.with_gateway(gw);
    // 本地 chat mock：story + storyboard 两响应
    let (chat_port, _ch) = spawn_mock_upstream(vec![
        chat_response("【第一幕】剧情正文一。\n【第二幕】剧情正文二。"),
        chat_response(&serde_json::json!([
            {"shot":1,"desc":"d","image_prompt":"p","video_prompt":"v","line":"台词","duration_secs":5}
        ]).to_string()),
    ]);
    h = h.with_local_chat(chat_port, "qwen-test");
    // 生图/ffmpeg 假件
    let fixture = temp_dir_for("cost-fix");
    let smi = fake_exec(&fixture, "fake-smi.sh", "#!/bin/sh\necho 24000\n");
    let imggen = fake_exec(
        &fixture,
        "fake-imggen.sh",
        "#!/bin/sh\nprintf '\\211PNG\\015\\012\\032\\012film' > \"$NEXOS_IMGGEN_OUT\"\n",
    );
    h = h.with_imggen_mock(
        imggen.to_str().unwrap(),
        fixture.join("fake-imggen.sh").to_str().unwrap(),
        smi.to_str().unwrap(),
    );
    let ff = fake_exec(
        &fixture,
        "ffmpeg",
        "#!/bin/sh\nout=\"\"\nfor a in \"$@\"; do out=\"$a\"; done\n: > \"$out\"\nexit 0\n",
    );
    h = h.with_ffmpeg_bin(ff.to_str().unwrap());

    let (id, dir) = create_project(&h, "16:9").await;
    // story → storyboard（本地）
    let (t1, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/story/generate"),
        serde_json::json!({"model_ref": {"source": "local", "capability": "chat"}}),
    )
    .await;
    let (t2, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/storyboard/generate"),
        serde_json::json!({"model_ref": {"source": "local", "capability": "chat"}}),
    )
    .await;
    assert_eq!(t1["status"], "done");
    assert_eq!(t2["status"], "done");
    // image（本地）→ commit；video（渠道）→ commit
    let (ti, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/shots/1/image"),
        serde_json::json!({"model_ref": {"source": "local", "capability": "image"}}),
    )
    .await;
    assert_eq!(ti["status"], "done");
    h.handle(post_req(
        &format!("/api/v1/film/projects/{id}/cache/shot-1.png/commit"),
        serde_json::json!({}),
    ))
    .await
    .unwrap();
    let (tv, _) = run_stage(&h, &format!("/api/v1/film/projects/{id}/shots/1/video"),
        serde_json::json!({"model_ref": {"source": "channel", "channel_id": ch_id, "capability": "video"}})).await;
    assert_eq!(tv["status"], "done");
    h.handle(post_req(
        &format!("/api/v1/film/projects/{id}/cache/shot-1.mp4/commit"),
        serde_json::json!({}),
    ))
    .await
    .unwrap();
    // tts + music（渠道）
    let (tt, _) = run_stage(&h, &format!("/api/v1/film/projects/{id}/shots/1/tts"),
        serde_json::json!({"model_ref": {"source": "channel", "channel_id": ch_id, "capability": "tts"}})).await;
    let (tm, _) = run_stage(&h, &format!("/api/v1/film/projects/{id}/music"),
        serde_json::json!({"model_ref": {"source": "channel", "channel_id": ch_id, "capability": "music"}})).await;
    assert_eq!(tt["status"], "done");
    assert_eq!(tm["status"], "done");
    // compose（本地 ffmpeg）
    let (tc, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/compose"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(tc["status"], "done", "{tc:?}");

    // by=stage：六阶段齐（story/storyboard/image/video/tts/music/compose）
    let resp = h
        .handle(get_req(&format!("/api/v1/film/projects/{id}/cost")))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "{resp:?}");
    let keys: Vec<&str> = resp.body["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["key"].as_str().unwrap())
        .collect();
    for stage in [
        "story",
        "storyboard",
        "image",
        "video",
        "tts",
        "music",
        "compose",
    ] {
        assert!(keys.contains(&stage), "缺 {stage} 埋点: {keys:?}");
    }
    assert!(resp.body["events"].as_u64().unwrap() >= 7);
    assert!(
        resp.body["totals"]["tokens"].as_u64().unwrap() > 0,
        "chat usage 记 tokens"
    );
    assert!(resp.body["totals"]["bytes"].as_u64().unwrap() > 0);
    // 渠道单价：by=channel 渠道组 cost=1.5×4（video/tts/music + …4 次渠道调用）
    let resp = h
        .handle(get_req(&format!(
            "/api/v1/film/projects/{id}/cost?by=channel"
        )))
        .await
        .unwrap();
    let ch_group = resp.body["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["key"] == ch_id.as_str())
        .expect("渠道组");
    assert_eq!(
        ch_group["cost"].as_f64().unwrap(),
        4.5,
        "1.5×3 次渠道调用（video/tts/music）: {ch_group}"
    );
    assert_eq!(ch_group["events"].as_u64().unwrap(), 3);
    let local_group = resp.body["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["key"] == "local")
        .expect("local 组");
    assert_eq!(
        local_group["cost"].as_f64().unwrap(),
        0.0,
        "本地未配单价只计量"
    );
    // by=day 单日一组；by=bad 400
    let resp = h
        .handle(get_req(&format!("/api/v1/film/projects/{id}/cost?by=day")))
        .await
        .unwrap();
    assert_eq!(resp.body["groups"].as_array().unwrap().len(), 1);
    let resp = h
        .handle(get_req(&format!(
            "/api/v1/film/projects/{id}/cost?by=model"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status, 400, "{resp:?}");
    // budget.json 投影与 DB 事件数一致
    let budget = read_json(&format!("{dir}/hub/budget.json"));
    assert!(
        budget["events"].as_array().unwrap().len() >= 7,
        "账本投影: {budget}"
    );
    for e in budget["events"].as_array().unwrap() {
        assert!(e["stage"].is_string() && e["created_at"].is_string());
        assert!(e["wall_secs"].as_f64().unwrap() >= 0.0);
    }
}

// ---------------------------------------------------------------------------
// export：显式导出不覆盖 story/casting/ownership/activity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn export_refreshes_state_but_preserves_file_truth() {
    let (h, _dir) = handler_at("export");
    let (id, dir) = create_project(&h, "16:9").await;
    let root = format!("{dir}/hub");
    // 手工放文件真值
    seed_story_md(&dir, "【第一幕】手工剧情。");
    std::fs::create_dir_all(format!("{root}/casting/characters/小明/views")).unwrap();
    std::fs::write(
        format!("{root}/casting/characters/小明/card.md"),
        "---\nname: 小明\n---\n手工卡",
    )
    .unwrap();
    let own = serde_json::json!({"members": [{"name": "alice", "joined_at": "t"}], "sections": {}, "casting_objects": {}});
    std::fs::write(
        format!("{root}/ownership.json"),
        serde_json::to_string_pretty(&own).unwrap(),
    )
    .unwrap();
    std::fs::write(
        format!("{root}/activity.json"),
        r#"[{"ts":"t","author":"a","action":"x","target":"y"}]"#,
    )
    .unwrap();
    seed_script(&dir, vec![shot_json(1, "镜头一", 5)]);
    let resp = h
        .handle(post_req(
            &format!("/api/v1/film/projects/{id}/export"),
            serde_json::json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200, "{resp:?}");
    let written: Vec<&str> = resp.body["written"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap())
        .collect();
    assert!(written.contains(&"project.md"), "{written:?}");
    assert!(
        written.contains(&"storyboard/storyboard.json"),
        "script.json 较新 → 平移: {written:?}"
    );
    // 文件真值保留
    assert!(std::fs::read_to_string(format!("{root}/story/story.md"))
        .unwrap()
        .contains("手工剧情"));
    assert!(Path::new(&format!("{root}/casting/characters/小明/card.md")).is_file());
    assert_eq!(
        read_json(&format!("{root}/ownership.json"))["members"][0]["name"],
        "alice"
    );
    let acts0 = activity_list(&dir);
    assert_eq!(acts0[0]["action"], "x", "手工 activity 原样保留: {acts0:?}");
    assert!(
        acts0.iter().any(|a| a["action"] == "export"),
        "export 完成点落流水: {acts0:?}"
    );
    // 再 export（带 author）追加一条
    let resp = h
        .handle(post_req(
            &format!("/api/v1/film/projects/{id}/export"),
            serde_json::json!({"author": "alice"}),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    let acts = activity_list(&dir);
    assert!(
        acts.iter()
            .filter(|a| a["action"] == "export" && a["author"] == "alice")
            .count()
            == 1,
        "{acts:?}"
    );
    assert_eq!(acts[0]["action"], "x", "首条仍是手工流水（环形不前插）");
}

// ---------------------------------------------------------------------------
// v0.1.37 story 文档处理管线（清理 / 分章 / 人物梳理）
// ---------------------------------------------------------------------------

/// 按请求**内容**选响应的 mock 上游（分块并发≤2 时 accept 顺序不定——
/// 响应由请求里的块标记决定，与连接顺序无关）。
fn spawn_mock_upstream_by(
    mut respond: impl FnMut(&str) -> Vec<u8> + Send + 'static,
) -> (u16, Arc<StdMutex<Vec<String>>>) {
    let hits: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(vec![]));
    let hits2 = Arc::clone(&hits);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buf);
                        if let Some(hend) = text.find("\r\n\r\n") {
                            let cl = text[..hend]
                                .lines()
                                .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                                .and_then(|l| l.split(':').nth(1))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            if buf.len() >= hend + 4 + cl {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            let req = String::from_utf8_lossy(&buf).into_owned();
            hits2.lock().unwrap().push(req.clone());
            let body = respond(&req);
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
            let _ = stream.flush();
        }
    });
    (port, hits)
}

#[test]
fn parse_mb_limit_covers_default_zero_garbage() {
    assert_eq!(parse_mb_limit("", 64), 64, "空值回缺省");
    assert_eq!(parse_mb_limit(" 8 ", 64), 8, "空白容忍");
    assert_eq!(parse_mb_limit("0", 64), 0, "0=无限标记");
    assert_eq!(parse_mb_limit("abc", 64), 64, "垃圾值回缺省");
    assert_eq!(parse_mb_limit("-1", 64), 64, "负数解析失败回缺省");
    // env 缺省口径：未设 env 时上限=64MB（超宽护栏；0 才是无限）
    assert_eq!(story_import_max_bytes(), Some(64 * 1024 * 1024));
}

#[test]
fn clean_text_rules_strips_ads_blanks_and_punct() {
    let text = "\n\n  第一章 风起  \n更多精彩请访问 www.abc.com\n\n\n小说正文,清晨的雾:他喊了一声!\n英文 keep a,b: c? ok.\n\n求收藏求月票\n";
    let (out, report) = clean_text_rules(text);
    assert!(!out.contains("www.abc.com") && !out.contains("更多精彩"), "{out}");
    assert!(!out.contains("求收藏"), "{out}");
    assert!(out.starts_with("第一章 风起"), "首尾空行+行首空白归一: {out:?}");
    assert!(
        out.contains("小说正文，清晨的雾：他喊了一声！"),
        "CJK 邻接标点归一: {out}"
    );
    assert!(out.contains("keep a,b: c? ok."), "纯 ASCII 语境不动: {out}");
    assert_eq!(report.ad_lines_removed, 2, "两条广告行");
    assert_eq!(report.chars_out, out.chars().count(), "报告口径一致");
    assert!(report.blank_lines_removed >= 2, "空行折叠+首尾去除: {report:?}");
    assert!(!out.contains("\n\n\n"), "连续空行折叠为单空行");
}

#[test]
fn split_chunks_by_lines_tracks_absolute_lines() {
    let mut text = String::new();
    for i in 0..30 {
        text.push_str(&format!("第{}行内容，稍微长一点。\n", i + 1));
        if i % 5 == 4 {
            text.push('\n');
        }
    }
    let chunks = split_chunks_by_lines(&text, 60);
    assert!(chunks.len() >= 2, "应分多块: {}", chunks.len());
    for w in chunks.windows(2) {
        assert!(
            w[1].start_line > w[0].start_line,
            "起始行单调: {:?}",
            chunks.iter().map(|c| c.start_line).collect::<Vec<_>>()
        );
    }
    assert!(chunks[0].text.starts_with("第1行内容"), "首块从原文首行起");
}

#[test]
fn chapter_break_parse_and_locate() {
    let raw = "```json\n{\"chapters\":[{\"no\":1,\"title\":\"第一章 风起\",\"quote\":\"清晨的雾漫过码头\"},{\"no\":2,\"title\":\"第二章 云涌\",\"quote\":\"暴风雨在午夜抵达\"}]}\n```";
    let raws = parse_chapter_breaks(raw).unwrap();
    assert_eq!(raws.len(), 2);
    assert_eq!(raws[0].title, "第一章 风起");
    assert!(parse_chapter_breaks("完全不是 JSON").is_err());
    let lines = vec![
        "楔子：旧海图",
        "第一章 风起",
        "清晨的雾漫过码头，船帆次第升起。",
        "水手们喊着号子。",
        "第二章 云涌",
        "暴风雨在午夜抵达。",
    ];
    let located = locate_chapter_lines(&lines, &raws);
    assert_eq!(located.len(), 2);
    assert_eq!(located[0].0, 1, "标题行定位");
    assert_eq!(located[1].0, 4);
    // 空引文回退标题匹配；断点乱序（已被前章覆盖）跳过
    let raws2 = vec![
        RawChapter { no: 1, title: "不存在的一章".into(), quote: "不存在的引文".into() },
        RawChapter { no: 2, title: "第二章 云涌".into(), quote: String::new() },
    ];
    let located2 = locate_chapter_lines(&lines, &raws2);
    assert_eq!(located2.len(), 1, "找不到的断点跳过: {located2:?}");
    assert_eq!(located2[0].0, 4);
}

#[test]
fn auto_segment_splits_near_target() {
    let mut text = String::new();
    for p in 0..6 {
        let body = "雨夜霓虹灯下的猫走过长街。".repeat(160);
        text.push_str(&format!("段落{p}开头\n{body}\n\n"));
    }
    let segs = auto_segment(&text, 4_000);
    assert!(segs.len() >= 2 && segs.len() <= 4, "分段数合理: {segs:?}");
    for w in segs.windows(2) {
        assert!(w[1].0 > w[0].1, "区间连续不重叠: {segs:?}");
    }
    assert_eq!(segs[0].0, 1, "从首行起");
    let total = text.lines().count();
    assert!(segs.last().unwrap().1 <= total, "不越界: {segs:?} / {total}");
}

#[test]
fn profile_chunk_parse_and_alias_merge() {
    let raw = "{\"characters\":[\
        {\"name\":\"小明\",\"aliases\":[\"明明\"],\"gender\":\"男\",\"age\":17,\"appearance\":\"黑短发\",\"personality\":\"勇敢\",\"relations\":[{\"name\":\"阿蓝\",\"relation\":\"挚友\"}],\"arc\":\"块1弧线\"},\
        {\"name\":\"明明\",\"appearance\":\"黑短发高个\",\"arc\":\"块2弧线\"},\
        {\"name\":\"阿蓝\",\"gender\":\"女\"}]}";
    let es = parse_profile_chunk(raw).unwrap();
    assert_eq!(es.len(), 3);
    assert_eq!(es[0].age, "17", "数字字段宽容转字符串");
    let mut all = es.clone();
    all.extend(es.clone()); // 跨块重复出现也归并
    let merged = merge_character_profiles(all);
    assert_eq!(
        merged.len(),
        2,
        "别名归并: {:?}",
        merged.iter().map(|m| m.name.clone()).collect::<Vec<_>>()
    );
    let xm = merged.iter().find(|m| m.name == "小明").unwrap();
    assert!(xm.aliases.contains(&"明明".to_string()), "{xm:?}");
    assert!(xm.arc.contains("块1弧线") && xm.arc.contains("块2弧线"), "{xm:?}");
    assert_eq!(xm.relations.len(), 1);
    // 根数组形态也认
    let arr = "[{\"name\":\"乙\"}]";
    assert_eq!(parse_profile_chunk(arr).unwrap().len(), 1);
    assert!(parse_profile_chunk("噪音").is_err());
    // 排序：first_chapter 小者在前，缺省推后
    let seeded = vec![
        ProfileEntry { name: "无章者".into(), ..ProfileEntry::default() },
        ProfileEntry { name: "乙".into(), first_chapter: Some(3), ..ProfileEntry::default() },
        ProfileEntry { name: "甲".into(), first_chapter: Some(1), ..ProfileEntry::default() },
    ];
    let m2 = merge_character_profiles(seeded);
    assert_eq!(m2[0].name, "甲");
    assert_eq!(m2[2].name, "无章者");
}

#[test]
fn source_base_slug_and_chapter_range() {
    assert_eq!(source_base_slug("story/source-小说原文.txt"), "小说原文");
    assert_eq!(source_base_slug("story/cleaned-novel.txt"), "novel");
    assert_eq!(source_base_slug("story/x.md"), "x");
    assert_eq!(parse_chapter_range("1-5"), Some((1, 5)));
    assert_eq!(parse_chapter_range("3"), Some((3, 3)));
    assert_eq!(parse_chapter_range(" 2-4 "), Some((2, 4)));
    assert_eq!(parse_chapter_range("5-1"), None, "倒序非法");
    assert_eq!(parse_chapter_range("x"), None);
}

#[tokio::test]
async fn story_import_3mb_passes_under_default_guard() {
    let (h, _dir) = handler_at("story-import-3mb");
    let (id, dir) = create_project(&h, "16:9").await;
    // 3MB UTF-8 文本（用户被拒场景：旧 2MB 上限 → 现缺省 64MB 护栏下直过）
    let big = "a".repeat(3 * 1024 * 1024);
    let resp = h
        .handle(post_req(
            &format!("/api/v1/film/projects/{id}/story/import"),
            serde_json::json!({
                "filename": "big-novel.txt",
                "content_b64": b64(big.as_bytes()),
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 201, "3MB 应直过: {resp:?}");
    assert_eq!(resp.body["chars"], 3 * 1024 * 1024);
    let meta = std::fs::metadata(format!("{dir}/hub/story/source-big-novel.txt")).unwrap();
    assert_eq!(meta.len(), (3 * 1024 * 1024) as u64);
}

#[tokio::test]
async fn story_clean_rules_pipeline_writes_cleaned_and_report() {
    let (h, _dir) = handler_at("story-clean-rules");
    let (id, dir) = create_project(&h, "16:9").await;
    let raw = "\n\n  第一章 起点  \n\n\n更多精彩请访问 www.x.com\n正文一:猫叫了一声!\n\n\n求收藏\n结尾段。\n";
    h.handle(post_req(
        &format!("/api/v1/film/projects/{id}/story/import"),
        serde_json::json!({
            "filename": "novel.txt",
            "content_b64": b64(raw.as_bytes()),
            "author": "alice",
        }),
    ))
    .await
    .unwrap();
    // rules 模式无需 model_ref（本地零成本）
    let (task, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/story/clean"),
        serde_json::json!({"source_file": "novel.txt", "mode": "rules", "author": "alice"}),
    )
    .await;
    assert_eq!(task["status"], "done", "{task:?}");
    let cleaned = std::fs::read_to_string(format!("{dir}/hub/story/cleaned-novel.txt")).unwrap();
    assert!(cleaned.contains("正文一：猫叫了一声！"), "{cleaned}");
    assert!(!cleaned.contains("www.x.com") && !cleaned.contains("求收藏"), "{cleaned}");
    let report = read_json(&format!("{dir}/hub/story/cleaned-novel.report.json"));
    assert_eq!(report["source"], "story/source-novel.txt");
    assert_eq!(report["mode"], "rules");
    assert_eq!(report["report"]["ad_lines_removed"], 2, "{report}");
    // activity 三 action 之一：story.clean（author 留名）
    let acts = activity_list(&dir);
    assert!(
        acts.iter()
            .any(|a| a["action"] == "story.clean" && a["author"] == "alice"),
        "{acts:?}"
    );
    // README 阶段仍 story（进度推进不跳阶段）
    let (rfm, _) = split_front_matter(
        &std::fs::read_to_string(format!("{dir}/hub/README.md")).unwrap(),
    );
    assert_eq!(rfm.get("stage").unwrap(), "story");
    // 校验矩阵：非法 mode / llm 缺 model_ref → 400
    for (body, mark) in [
        (
            serde_json::json!({"source_file": "novel.txt", "mode": "deep"}),
            "非法 mode",
        ),
        (
            serde_json::json!({"source_file": "novel.txt", "mode": "llm"}),
            "llm 缺 model_ref",
        ),
        (serde_json::json!({"mode": "rules"}), "缺 source_file"),
    ] {
        let resp = h
            .handle(post_req(&format!("/api/v1/film/projects/{id}/story/clean"), body))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "{mark} 应 400: {resp:?}");
    }
    // 源不存在 → 任务 error 如实
    let (task2, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/story/clean"),
        serde_json::json!({"source_file": "不存在的.txt", "mode": "rules"}),
    )
    .await;
    assert_eq!(task2["status"], "error", "{task2:?}");
}

#[tokio::test]
async fn story_clean_llm_chunks_assemble_with_fence_strip() {
    let (mut h, _dir) = handler_at("story-clean-llm");
    // 2 块语料（>8K 字符触发分块；块内容带 ASCII 标记供 mock 按请求选响应）
    let mut text = String::from("AAAKEYY 序章。\n");
    text.push_str(&"序章正文填充。".repeat(700));
    text.push_str("\n\nBBBKEY 后段。\n");
    text.push_str(&"后段正文填充。".repeat(700));
    let (port, hits) = spawn_mock_upstream_by(|req| {
        if req.contains("AAAKEYY") {
            chat_response("```text\nA块已清理正文\n```")
        } else if req.contains("BBBKEY") {
            chat_response("B块已清理正文")
        } else {
            chat_response("{}")
        }
    });
    h = h.with_local_chat(port, "qwen-test");
    let (id, dir) = create_project(&h, "16:9").await;
    h.handle(post_req(
        &format!("/api/v1/film/projects/{id}/story/import"),
        serde_json::json!({
            "filename": "novel.txt",
            "content_b64": b64(text.as_bytes()),
        }),
    ))
    .await
    .unwrap();
    let (task, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/story/clean"),
        serde_json::json!({
            "source_file": "novel.txt",
            "mode": "llm",
            "model_ref": {"source": "local", "capability": "chat"},
        }),
    )
    .await;
    assert_eq!(task["status"], "done", "{task:?}");
    let cleaned = std::fs::read_to_string(format!("{dir}/hub/story/cleaned-novel.txt")).unwrap();
    assert!(
        cleaned.contains("A块已清理正文") && cleaned.contains("B块已清理正文"),
        "{cleaned}"
    );
    assert!(!cleaned.contains("```"), "围栏剥除: {cleaned}");
    // 两块都真实喂了模型（含规则清理后的块标记）
    assert!(hits.lock().unwrap().len() >= 2, "分块调用: {}", hits.lock().unwrap().len());
    // 报告 mode=llm
    let report = read_json(&format!("{dir}/hub/story/cleaned-novel.report.json"));
    assert_eq!(report["mode"], "llm");
}

#[tokio::test]
async fn story_chapterize_three_chunks_merge_and_write_files() {
    let (mut h, _dir) = handler_at("story-chapterize");
    // 3 块语料：每块一章标题开头（标题同时是定位锚；正文填充不含块标记——
    // 块重叠上下文只带上一块尾部，防 mock 按内容选响应时串块）
    let mut text = String::new();
    for (i, key) in ["CKEY1", "CKEY2", "CKEY3"].iter().enumerate() {
        let titles = ["第一章 风起", "第二章 云涌", "第三章 落幕"];
        text.push_str(&format!("{} {}\n", titles[i], key));
        text.push_str(&"章内正文填充句子。".repeat(450));
        text.push_str("\n\n");
    }
    let (port, hits) = spawn_mock_upstream_by(|req| {
        let out = if req.contains("CKEY1") {
            r#"{"chapters":[{"no":1,"title":"第一章 风起","quote":""}]}"#
        } else if req.contains("CKEY2") {
            r#"{"chapters":[{"no":2,"title":"第二章 云涌","quote":""}]}"#
        } else if req.contains("CKEY3") {
            r#"{"chapters":[{"no":3,"title":"第三章 落幕","quote":""}]}"#
        } else {
            r#"{"chapters":[]}"#
        };
        chat_response(out)
    });
    h = h.with_local_chat(port, "qwen-test");
    let (id, dir) = create_project(&h, "16:9").await;
    h.handle(post_req(
        &format!("/api/v1/film/projects/{id}/story/import"),
        serde_json::json!({
            "filename": "novel.txt",
            "content_b64": b64(text.as_bytes()),
        }),
    ))
    .await
    .unwrap();
    let (task, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/story/chapterize"),
        serde_json::json!({
            "model_ref": {"source": "local", "capability": "chat"},
            "source_file": "novel.txt",
            "author": "alice",
        }),
    )
    .await;
    assert_eq!(task["status"], "done", "{task:?}");
    // 三块全部喂模型（并发≤2，重试 0）
    assert_eq!(hits.lock().unwrap().len(), 3, "{:?}", hits.lock().unwrap().len());
    let index = read_json(&format!("{dir}/hub/story/chapters/index.json"));
    assert_eq!(index["source"], "story/source-novel.txt");
    assert_eq!(index["auto"], false, "{index}");
    let chapters = index["chapters"].as_array().unwrap();
    assert_eq!(chapters.len(), 3, "{index}");
    assert_eq!(chapters[0]["title"], "第一章 风起");
    assert_eq!(chapters[0]["start_line"], 1, "首章从首行起: {index}");
    for w in chapters.windows(2) {
        assert!(
            w[1]["start_line"].as_u64().unwrap() > w[0]["end_line"].as_u64().unwrap()
                || w[1]["start_line"].as_u64().unwrap() == w[0]["end_line"].as_u64().unwrap() + 1,
            "区间衔接: {index}"
        );
    }
    for (i, ch) in chapters.iter().enumerate() {
        assert!(ch["words"].as_u64().unwrap() > 1000, "章字数: {ch}");
        let file = ch["file"].as_str().unwrap();
        assert_eq!(file, format!("story/chapters/ch-{:02}.md", i + 1));
        let doc = std::fs::read_to_string(format!("{dir}/hub/{file}")).unwrap();
        let (fm, body) = split_front_matter(&doc);
        assert_eq!(fm.get("no").unwrap(), &(i + 1).to_string());
        assert!(body.contains("正文"), "章正文: {doc:?}");
    }
    let acts = activity_list(&dir);
    assert!(
        acts.iter()
            .any(|a| a["action"] == "story.chapterize" && a["author"] == "alice"
                && a["target"] == "story/chapters/index.json"),
        "{acts:?}"
    );
}

#[tokio::test]
async fn story_chapterize_auto_segments_when_llm_finds_nothing() {
    let (mut h, _dir) = handler_at("story-chapterize-auto");
    // 无章节结构长文（>2×4K 字符触发自动分段）
    let mut text = String::new();
    for p in 0..8 {
        text.push_str(&format!("自动段{p}标记\n"));
        text.push_str(&"雨夜长街正文填充。".repeat(260));
        text.push_str("\n\n");
    }
    let (port, _) = spawn_mock_upstream_by(|_| chat_response(r#"{"chapters":[]}"#));
    h = h.with_local_chat(port, "qwen-test");
    let (id, dir) = create_project(&h, "16:9").await;
    h.handle(post_req(
        &format!("/api/v1/film/projects/{id}/story/import"),
        serde_json::json!({
            "filename": "novel.txt",
            "content_b64": b64(text.as_bytes()),
        }),
    ))
    .await
    .unwrap();
    let (task, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/story/chapterize"),
        serde_json::json!({"model_ref": {"source": "local", "capability": "chat"}}),
    )
    .await;
    assert_eq!(task["status"], "done", "{task:?}");
    let index = read_json(&format!("{dir}/hub/story/chapters/index.json"));
    assert_eq!(index["auto"], true, "自动分段标注: {index}");
    let chapters = index["chapters"].as_array().unwrap();
    assert!(chapters.len() >= 2, "分段数: {index}");
    assert!(
        chapters[0]["title"].as_str().unwrap().starts_with("第1部分"),
        "自动分段标题: {index}"
    );
    assert_eq!(chapters[0]["auto"], true);
}

#[tokio::test]
async fn story_profile_merges_alias_across_chunks_with_first_chapter() {
    let (mut h, _dir) = handler_at("story-profile");
    // 语料 2 块（>6K 字符）；预置章节 index（first_chapter 行号映射源）
    let mut text = String::from("PKEY1 开篇，小明登场。\n");
    text.push_str(&"开篇正文填充。".repeat(500));
    text.push_str("\n\nPKEY2 后段，明明与阿蓝同行。\n");
    text.push_str(&"后段正文填充。".repeat(500));
    let (port, _) = spawn_mock_upstream_by(|req| {
        if req.contains("PKEY1") {
            chat_response(
                r#"{"characters":[{"name":"小明","aliases":["明明"],"gender":"男","age":"17","appearance":"黑短发","personality":"沉静","relations":[{"name":"阿蓝","relation":"挚友"}],"arc":"块1寻找灯塔"}]}"#,
            )
        } else {
            chat_response(
                r#"{"characters":[{"name":"明明","appearance":"黑短发，背旧书包","arc":"块2渡海"},{"name":"阿蓝","gender":"女","personality":"爽朗"}]}"#,
            )
        }
    });
    h = h.with_local_chat(port, "qwen-test");
    let (id, dir) = create_project(&h, "16:9").await;
    // 直接种章节 index + 章文件（行号：1-2 章1，3 起章2——与上方语料行布局对齐）
    std::fs::create_dir_all(format!("{dir}/hub/story/chapters")).unwrap();
    std::fs::write(
        format!("{dir}/hub/story/chapters/index.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "version": 1,
            "source": "story/source-novel.txt",
            "auto": false,
            "chapters": [
                {"no": 1, "title": "第一章", "start_line": 1, "end_line": 2, "words": 3000, "file": "story/chapters/ch-01.md"},
                {"no": 2, "title": "第二章", "start_line": 3, "end_line": 999, "words": 3000, "file": "story/chapters/ch-02.md"},
            ],
        }))
        .unwrap(),
    )
    .unwrap();
    h.handle(post_req(
        &format!("/api/v1/film/projects/{id}/story/import"),
        serde_json::json!({
            "filename": "novel.txt",
            "content_b64": b64(text.as_bytes()),
        }),
    ))
    .await
    .unwrap();
    let (task, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/story/profile"),
        serde_json::json!({
            "model_ref": {"source": "local", "capability": "chat"},
            "source_file": "novel.txt",
            "author": "alice",
        }),
    )
    .await;
    assert_eq!(task["status"], "done", "{task:?}");
    let profiles: Vec<Value> = read_json(&format!("{dir}/hub/story/characters-profile.json"))
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(profiles.len(), 2, "别名归并: {profiles:?}");
    let xm = profiles.iter().find(|p| p["name"] == "小明").unwrap();
    assert!(xm["aliases"].as_array().unwrap().iter().any(|a| a == "明明"), "{xm:?}");
    assert!(
        xm["appearance"]
            .as_str()
            .unwrap()
            .contains("黑短发，背旧书包"),
        "外貌去重拼接: {xm:?}"
    );
    assert_eq!(xm["first_chapter"], 1, "首现章（两块取最小）: {xm:?}");
    assert!(xm["arc"].as_str().unwrap().contains("块1寻找灯塔"), "{xm:?}");
    let al = profiles.iter().find(|p| p["name"] == "阿蓝").unwrap();
    assert_eq!(al["first_chapter"], 2, "{al:?}");
    let acts = activity_list(&dir);
    assert!(
        acts.iter()
            .any(|a| a["action"] == "story.profile" && a["author"] == "alice"
                && a["target"] == "story/characters-profile.json"),
        "{acts:?}"
    );
    // 排序：小明（首现 1）在阿蓝（首现 2）前
    assert_eq!(profiles[0]["name"], "小明", "{profiles:?}");
}

#[tokio::test]
async fn story_profile_chapter_range_reads_chapter_files() {
    let (mut h, _dir) = handler_at("story-profile-range");
    let (port, hits) = spawn_mock_upstream_by(|req| {
        if req.contains("RKEY1") {
            chat_response(r#"{"characters":[{"name":"甲","arc":"仅第一章"}]}"#)
        } else {
            chat_response(r#"{"characters":[]}"#)
        }
    });
    h = h.with_local_chat(port, "qwen-test");
    let (id, dir) = create_project(&h, "16:9").await;
    // 章文件直种（范围语料来自 ch 文件，不走 source）
    std::fs::create_dir_all(format!("{dir}/hub/story/chapters")).unwrap();
    for (no, key) in [(1u32, "RKEY1"), (2, "RKEY2")] {
        let body = format!("{key} 章正文。\n");
        let mut fm = BTreeMap::new();
        fm.insert("no".to_string(), no.to_string());
        fm.insert("title".to_string(), format!("第{no}章"));
        fm.insert("words".to_string(), body.chars().count().to_string());
        std::fs::write(
            format!("{dir}/hub/story/chapters/ch-{no:02}.md"),
            render_doc(&fm, &body),
        )
        .unwrap();
    }
    std::fs::write(
        format!("{dir}/hub/story/chapters/index.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "version": 1,
            "source": "story/source-novel.txt",
            "chapters": [
                {"no": 1, "title": "第一章", "start_line": 1, "end_line": 2, "words": 9, "file": "story/chapters/ch-01.md"},
                {"no": 2, "title": "第二章", "start_line": 3, "end_line": 4, "words": 9, "file": "story/chapters/ch-02.md"},
            ],
        }))
        .unwrap(),
    )
    .unwrap();
    let (task, _) = run_stage(
        &h,
        &format!("/api/v1/film/projects/{id}/story/profile"),
        serde_json::json!({
            "model_ref": {"source": "local", "capability": "chat"},
            "chapter_range": "1",
            "author": "alice",
        }),
    )
    .await;
    assert_eq!(task["status"], "done", "{task:?}");
    let profiles = read_json(&format!("{dir}/hub/story/characters-profile.json"));
    let arr = profiles.as_array().unwrap();
    assert_eq!(arr.len(), 1, "{profiles:?}");
    assert_eq!(arr[0]["name"], "甲");
    // 只读了第 1 章内容（请求含 RKEY1 不含 RKEY2）
    let reqs = hits.lock().unwrap().clone();
    assert!(
        reqs.iter().any(|r| r.contains("RKEY1")) && reqs.iter().all(|r| !r.contains("RKEY2")),
        "范围语料裁剪: {reqs:?}"
    );
    // 非法 range → 400
    let resp = h
        .handle(post_req(
            &format!("/api/v1/film/projects/{id}/story/profile"),
            serde_json::json!({
                "model_ref": {"source": "local", "capability": "chat"},
                "chapter_range": "5-1",
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status, 400, "{resp:?}");
}
