use openmouse_bridge::{
    config::{BridgeConfig, LocalApplication},
    local_targets,
};

use openmouse_bridge::{api, service::BridgeService};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

struct TestServer {
    url: String,
    path: PathBuf,
    service: BridgeService,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(self.path.parent().unwrap());
    }
}

async fn server(storage_failure: bool) -> TestServer {
    server_with_config(storage_failure, BridgeConfig::default()).await
}

async fn server_with_config(storage_failure: bool, config: BridgeConfig) -> TestServer {
    let directory = std::env::temp_dir().join(format!(
        "local-profile-test-{}-{}",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("config.json");
    if storage_failure {
        std::fs::create_dir(&path).unwrap();
    }
    let service = BridgeService::new(config, path.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = api::router(service.clone(), &[]);
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    TestServer {
        url,
        path,
        service,
        task,
    }
}

fn target() -> serde_json::Value {
    serde_json::json!({"id":"local:editor","name":"Editor","executable":"editor.exe","path":"C:/Tools/editor.exe","kind":"app"})
}

#[tokio::test]
async fn activation_metadata_roundtrips_into_canonical_profiles() {
    let server = server(false).await;
    let mut value = target();
    value["activation"] = "running".into();
    let application: LocalApplication = serde_json::from_value(value.clone()).unwrap();
    let response = reqwest::Client::new()
        .put(format!("{}/v1/local-applications", server.url))
        .json(&serde_json::json!({"applications":[application]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap(),
        serde_json::json!({"ok":true,"applications":[value.clone()]})
    );
    let stored = server.service.local_applications().await;
    assert_eq!(serde_json::to_value(&stored[0]).unwrap(), value);
    let catalog = server.service.games().await;
    let listed = catalog
        .iter()
        .find(|entry| entry["id"] == value["id"])
        .unwrap();
    for key in ["activation", "bundlePath", "bundleIdentifier"] {
        assert_eq!(listed[key], value[key]);
    }
    let mut profile = serde_json::from_value(serde_json::json!({
        "application":{"localId":"local:editor","name":"stale","executable":"stale","path":""},
        "device":{"id":"d","name":"Mouse"},"settings":{"dpi":800}
    }))
    .unwrap();
    local_targets::canonicalize_profile(&mut profile, &stored).unwrap();
    let metadata = serde_json::to_value(profile).unwrap();
    for key in ["activation", "bundlePath", "bundleIdentifier"] {
        assert_eq!(metadata["application"][key], value[key]);
    }
}

#[tokio::test]
async fn mismatched_bundle_metadata_rejects_atomically_with_profiles_intact() {
    let server = server(false).await;
    server
        .service
        .replace_local_applications(vec![serde_json::from_value(target()).unwrap()])
        .await
        .unwrap();
    let profile = serde_json::from_value(serde_json::json!({
        "application":{"localId":"local:editor","name":"Editor","executable":"editor.exe","path":""},
        "device":{"id":"d","name":"Mouse"},"settings":{"dpi":1600}
    })).unwrap();
    server
        .service
        .replace_profiles(vec![profile])
        .await
        .unwrap();
    let before = serde_json::to_value(server.service.config().await).unwrap();
    let disk = std::fs::read(&server.path).unwrap();
    for path in [
        "C:/Tools/editor.exe",
        "/opt/editor.exe",
        "/Applications/Editor.app/Contents/MacOS/editor.exe",
    ] {
        let mut value = target();
        value["path"] = path.into();
        value["bundlePath"] = "/Applications/Editor.app".into();
        value["bundleIdentifier"] = "io.editor".into();
        let response = reqwest::Client::new()
            .put(format!("{}/v1/local-applications", server.url))
            .json(&serde_json::json!({"applications":[value]}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "{path}");
        assert_eq!(
            serde_json::to_value(server.service.config().await).unwrap(),
            before
        );
        assert_eq!(std::fs::read(&server.path).unwrap(), disk);
    }
}

#[tokio::test]
async fn saved_missing_canonical_bundle_survives_batch_crud_and_profile_readback() {
    let bundle = serde_json::json!({"id":"local:missing", "name":"Missing", "executable":"editor", "path":"/Applications/AbsentEditor.app/Contents/MacOS/editor", "kind":"app", "bundlePath":"/Applications/AbsentEditor.app", "bundleIdentifier":"io.editor", "activation":"running"});
    let config = serde_json::from_value(serde_json::json!({"localApplications":[bundle.clone()], "profiles":[{
        "application":{"localId":"local:missing", "name":"Missing", "executable":"editor", "path":"/Applications/AbsentEditor.app/Contents/MacOS/editor"},
        "device":{"id":"d","name":"Mouse"}, "settings":{"dpi":1600}, "enabled":false
    }]})).unwrap();
    let server = server_with_config(false, config).await;
    let client = reqwest::Client::new();
    let url = format!("{}/v1/local-applications", server.url);
    for applications in [
        serde_json::json!([bundle.clone(), target()]),
        serde_json::json!([bundle.clone()]),
    ] {
        let response = client
            .put(&url)
            .json(&serde_json::json!({"applications":applications}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let readback: serde_json::Value = response.json().await.unwrap();
        assert_eq!(readback["applications"][0], bundle);
        let profiles = server.service.profiles().await;
        assert_eq!(profiles.len(), 1);
        assert_eq!(
            profiles[0].application.bundle_path.as_deref(),
            Some("/Applications/AbsentEditor.app")
        );
        assert_eq!(profiles[0].settings.dpi, Some(1600));
        assert!(!profiles[0].enabled);
    }
    let before = std::fs::read(&server.path).unwrap();
    let mut retargeted = bundle;
    retargeted["path"] = "/Applications/AbsentEditor.app/Contents/MacOS/other".into();
    retargeted["executable"] = "other".into();
    let response = client
        .put(&url)
        .json(&serde_json::json!({"applications":[retargeted]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(std::fs::read(&server.path).unwrap(), before);
    assert_eq!(server.service.profiles().await.len(), 1);
}

#[tokio::test]
async fn local_http_roundtrip_reconciles_id_linked_profiles_and_restarts() {
    let server = server(false).await;
    let client = reqwest::Client::new();
    let response = client
        .put(format!("{}/v1/local-applications", server.url))
        .json(&serde_json::json!({"applications":[target()]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap(),
        serde_json::json!({"ok":true,"applications":[target()]})
    );
    let local = serde_json::json!({"application":{"localId":"local:editor","name":"Wrong","executable":"wrong.exe","path":""},"device":{"id":"d","name":"Mouse"},"settings":{"dpi":1600},"enabled":false});
    let preset = serde_json::json!({"application":{"name":"Editor","executable":"editor.exe","path":""},"device":{"id":"d","name":"Mouse"},"settings":{"dpi":800}});
    let response = client
        .put(format!("{}/v1/profiles", server.url))
        .json(&serde_json::json!({"profiles":[local,preset]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let profiles = server.service.profiles().await;
    let profile = profiles
        .iter()
        .find(|profile| profile.application.local_id.is_some())
        .unwrap();
    assert_eq!(profile.application.name, "Editor");
    assert_eq!(profile.application.path, "C:/Tools/editor.exe");
    let mut renamed = target();
    renamed["name"] = "Renamed".into();
    let response = client
        .put(format!("{}/v1/local-applications", server.url))
        .json(&serde_json::json!({"applications":[renamed.clone()]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let profiles = server.service.profiles().await;
    let linked = profiles
        .iter()
        .find(|profile| profile.application.local_id.is_some())
        .unwrap();
    assert_eq!(linked.application.name, "Renamed");
    assert!(!linked.enabled);
    assert_eq!(linked.settings.dpi, Some(1600));
    let saved: BridgeConfig =
        serde_json::from_slice(&std::fs::read(&server.path).unwrap()).unwrap();
    let restarted = BridgeService::new(saved, server.path.clone());
    assert_eq!(restarted.profiles().await, profiles);
    let response = client
        .get(format!("{}/v1/games", server.url))
        .send()
        .await
        .unwrap();
    let catalog: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        catalog[0],
        serde_json::json!({"id":"local:editor","source":"local","name":"Renamed","executables":["editor.exe"],"path":"C:/Tools/editor.exe","kind":"app"})
    );
    let response = client
        .put(format!("{}/v1/local-applications", server.url))
        .json(&serde_json::json!({"applications":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let profiles = server.service.profiles().await;
    assert_eq!(profiles.len(), 1);
    assert!(profiles[0].application.local_id.is_none());
}

#[tokio::test]
async fn local_http_validation_is_400_and_never_changes_state() {
    let server = server(false).await;
    let client = reqwest::Client::new();
    let url = format!("{}/v1/local-applications", server.url);
    assert_eq!(
        client
            .put(&url)
            .json(&serde_json::json!({"applications":[target()]}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let original = std::fs::read(&server.path).unwrap();
    let mut batches = vec![
        serde_json::json!({}),
        serde_json::json!({"applications":[target(),target()]}),
    ];
    for (field, value) in [
        ("kind", "shell"),
        ("executable", "../bad.exe"),
        ("path", "relative/path.exe"),
        ("name", ""),
        ("id", "local:"),
        ("path", "C:/Other/wrong.exe"),
    ] {
        let mut invalid = target();
        invalid[field] = value.into();
        batches.push(serde_json::json!({"applications":[target(),invalid]}));
    }
    let mut missing = target();
    missing.as_object_mut().unwrap().remove("executable");
    batches.push(serde_json::json!({"applications":[missing]}));
    for payload in batches {
        let response = client.put(&url).json(&payload).send().await.unwrap();
        assert_eq!(response.status(), 400, "{payload}");
        assert_eq!(std::fs::read(&server.path).unwrap(), original);
        let targets: serde_json::Value =
            client.get(&url).send().await.unwrap().json().await.unwrap();
        assert_eq!(targets, serde_json::json!([target()]));
    }
    assert_eq!(
        client
            .put(&url)
            .header("content-type", "application/json")
            .body("{")
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    let unknown = serde_json::json!({"application":{"localId":"local:missing","name":"Editor","executable":"editor.exe","path":""},"device":{"id":"d","name":"Mouse"},"settings":{}});
    assert_eq!(
        client
            .put(format!("{}/v1/profiles", server.url))
            .json(&serde_json::json!({"profiles":[unknown]}))
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert!(server.service.profiles().await.is_empty());
    assert_eq!(std::fs::read(&server.path).unwrap(), original);
}

#[tokio::test]
async fn failed_storage_returns_500_without_changing_targets_or_profiles() {
    let server = server(true).await;
    let client = reqwest::Client::new();
    let before = serde_json::to_value(server.service.config().await).unwrap();
    let response = client
        .put(format!("{}/v1/local-applications", server.url))
        .json(&serde_json::json!({"applications":[target()]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 500);
    assert_eq!(
        serde_json::to_value(server.service.config().await).unwrap(),
        before
    );
    assert!(server.path.is_dir());
    let profile = serde_json::json!({"application":{"name":"Editor","executable":"editor.exe","path":""},"device":{"id":"d","name":"Mouse"},"settings":{}});
    let response = client
        .put(format!("{}/v1/profiles", server.url))
        .json(&serde_json::json!({"profiles":[profile]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 500);
    assert_eq!(
        serde_json::to_value(server.service.config().await).unwrap(),
        before
    );
    assert_eq!(
        std::fs::read_dir(server.path.parent().unwrap())
            .unwrap()
            .count(),
        1
    );
}

#[tokio::test]
async fn local_rename_delete_reconcile_only_linked_profiles_across_devices() {
    let server = server(false).await;
    let client = reqwest::Client::new();
    let mut other = target();
    other["id"] = "local:other".into();
    other["path"] = "C:/Other/editor.exe".into();
    let url = format!("{}/v1/local-applications", server.url);
    assert_eq!(
        client
            .put(&url)
            .json(&serde_json::json!({"applications":[target(),other.clone()]}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let make_profile = |id: Option<&str>, device: &str| {
        let mut application =
            serde_json::json!({"name":"Editor","executable":"editor.exe","path":""});
        if let Some(id) = id {
            application["localId"] = id.into();
        }
        serde_json::json!({"application":application,"device":{"id":device,"name":"Mouse"},"settings":{"dpi":800},"enabled":false})
    };
    let profiles = serde_json::json!([
        make_profile(Some("local:editor"), "one"),
        make_profile(Some("local:editor"), "two"),
        make_profile(Some("local:other"), "one"),
        make_profile(None, "one")
    ]);
    assert_eq!(
        client
            .put(format!("{}/v1/profiles", server.url))
            .json(&serde_json::json!({"profiles":profiles}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let mut renamed = target();
    renamed["name"] = "Renamed".into();
    renamed["executable"] = "renamed.exe".into();
    renamed["path"] = "C:/Tools/renamed.exe".into();
    assert_eq!(
        client
            .put(&url)
            .json(&serde_json::json!({"applications":[renamed,other.clone()]}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let profiles = server.service.profiles().await;
    assert_eq!(profiles.len(), 4);
    for profile in &profiles {
        assert!(!profile.enabled);
        assert_eq!(profile.settings.dpi, Some(800));
        if profile.application.local_id.as_deref() == Some("local:editor") {
            assert_eq!(profile.application.name, "Renamed");
            assert_eq!(profile.application.executable, "renamed.exe");
            assert_eq!(profile.application.path, "C:/Tools/renamed.exe");
        } else {
            assert_eq!(profile.application.name, "Editor");
        }
    }
    assert_eq!(
        client
            .put(&url)
            .json(&serde_json::json!({"applications":[other.clone()]}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let profiles = server.service.profiles().await;
    assert_eq!(profiles.len(), 2);
    assert!(
        profiles
            .iter()
            .all(|profile| profile.application.local_id.as_deref() != Some("local:editor"))
    );
    assert_eq!(
        client
            .put(format!("{}/v1/profiles", server.url))
            .json(&serde_json::json!({"profiles":[]}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(server.service.local_applications().await.len(), 1);
}

#[tokio::test]
async fn local_reconciliation_rolls_back_after_storage_failure() {
    let server = server(false).await;
    let client = reqwest::Client::new();
    let url = format!("{}/v1/local-applications", server.url);
    assert_eq!(
        client
            .put(&url)
            .json(&serde_json::json!({"applications":[target()]}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let profile = serde_json::json!({"application":{"localId":"local:editor","name":"Editor","executable":"editor.exe","path":""},"device":{"id":"d","name":"Mouse"},"settings":{"dpi":800}});
    assert_eq!(
        client
            .put(format!("{}/v1/profiles", server.url))
            .json(&serde_json::json!({"profiles":[profile]}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let before = serde_json::to_value(server.service.config().await).unwrap();
    let original = std::fs::read(&server.path).unwrap();
    let backup = server.path.with_extension("backup");
    std::fs::rename(&server.path, &backup).unwrap();
    std::fs::create_dir(&server.path).unwrap();
    let mut renamed = target();
    renamed["name"] = "Renamed".into();
    for payload in [
        serde_json::json!({"applications":[renamed]}),
        serde_json::json!({"applications":[]}),
    ] {
        assert_eq!(
            client
                .put(&url)
                .json(&payload)
                .send()
                .await
                .unwrap()
                .status(),
            500
        );
        assert_eq!(
            serde_json::to_value(server.service.config().await).unwrap(),
            before
        );
        assert_eq!(std::fs::read(&backup).unwrap(), original);
    }
}

#[tokio::test]
async fn failed_config_writes_preserve_all_in_memory_preferences() {
    let server = server(true).await;
    let before = serde_json::to_value(server.service.config().await).unwrap();
    assert!(server.service.set_battery_threshold(35).await.is_err());
    assert_eq!(
        serde_json::to_value(server.service.config().await).unwrap(),
        before
    );
    assert!(server.service.set_automatic_updates(true).await.is_err());
    assert!(server.service.set_notification_sound(false).await.is_err());
    assert!(server.service.set_notification_volume(15).await.is_err());
    assert!(server.service.acknowledge_tray_hint().await.is_err());
    assert_eq!(
        serde_json::to_value(server.service.config().await).unwrap(),
        before
    );
}

#[test]
fn local_batch_validation_rejects_duplicate_ids() {
    let target: LocalApplication = serde_json::from_value(serde_json::json!({"id":"local:editor","name":"Editor","executable":"editor.exe","path":"","kind":"app"})).unwrap();
    assert!(local_targets::validate(&[target.clone(), target]).is_err());
}

#[test]
fn local_id_profile_identity_is_persisted_without_colliding_with_preset() {
    let json = serde_json::json!({"profiles":[
        {"application":{"name":"Editor","executable":"editor.exe","path":"","localId":"local:editor"}, "device":{"id":"d","name":"Mouse"},"settings":{}},
        {"application":{"name":"Editor","executable":"editor.exe","path":""}, "device":{"id":"d","name":"Mouse"},"settings":{}}
    ]});
    let config: BridgeConfig = serde_json::from_value(json).unwrap();
    assert_eq!(config.normalized().profiles.len(), 2);
}
