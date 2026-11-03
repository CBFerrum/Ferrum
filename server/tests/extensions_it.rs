//! An app compiled in by a server built somewhere else (`ferrum_server::extensions`): its packages, manifest,
//! interface, hooks and tasks reach the server as a compiled-in app's do, and its hooks only for the workspaces that
//! install it. What a private app's repository relies on to build its own server.

use async_trait::async_trait;
use ferrum_kernel::HookRegistry;
use ferrum_server::extensions::{self, Extension};
use loco_rs::app::{AppContext, Hooks};
use loco_rs::task::{Task, TaskInfo, Tasks, Vars};
use std::collections::BTreeMap;
use std::sync::Arc;

const MANIFEST: &str = "id: elsewhere\nname: Elsewhere\nversion: 0.1.0\n";
static UI: &[(&str, &[u8])] = &[("index.html", b"<p>elsewhere</p>")];

fn packages() -> Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    let files: BTreeMap<String, String> = [
        ("package.yaml", "package: elsewhere\nversion: 0.1.0\ndepends: [core]\n"),
        ("entities/note.yaml", "name: ElsewhereNote\nlabel: Note\nkind: master\nfields:\n  - {name: title, type: data}\n"),
    ]
    .into_iter()
    .map(|(p, d)| (p.to_string(), d.to_string()))
    .collect();
    Ok(vec![ferrum_meta::catalog::load_package_files(&files)?])
}

struct Touch;

#[async_trait]
impl ferrum_kernel::DocHook for Touch {
    fn name(&self) -> &'static str {
        "elsewhere.touch"
    }
}

fn register(h: &mut HookRegistry) {
    h.register("ElsewhereNote", Arc::new(Touch));
}

struct Errand;

#[async_trait]
impl Task for Errand {
    fn task(&self) -> TaskInfo {
        TaskInfo { name: "elsewhere_errand".into(), detail: "What an app built in elsewhere adds".into() }
    }
    async fn run(&self, _: &AppContext, _: &Vars) -> loco_rs::Result<()> {
        Ok(())
    }
}

#[test]
fn an_app_built_in_elsewhere_reaches_the_server() {
    extensions::extend(vec![Extension {
        id: "elsewhere",
        packages,
        register,
        reports: Vec::new,
        maintenance: |_| Box::pin(async { Ok(()) }),
        manifest: MANIFEST,
        ui: UI,
        tasks: |t| t.register(Errand),
        metrics: |_| Box::pin(async { Ok("ferrum_elsewhere_up 1\n".to_string()) }),
    }]);
    let packages = ferrum_server::packages::native_packages().expect("packages");
    assert!(packages.iter().any(|p| p.name() == "elsewhere"), "its packages");
    assert!(ferrum_server::packages::native_manifests().contains(&MANIFEST), "its manifest");
    assert!(ferrum_server::packages::native_uis().iter().any(|(id, files)| *id == "elsewhere" && files.len() == 1), "its interface");

    let mut hooks = HookRegistry::default();
    ferrum_server::packages::register(&mut hooks);
    let scoped: Vec<_> = hooks.scopes().into_iter().filter(|(entity, _)| entity == "ElsewhereNote").collect();
    assert_eq!(scoped, vec![("ElsewhereNote".to_string(), Some(vec!["elsewhere".to_string()]))], "its hooks, for workspaces that install it");

    let mut tasks = Tasks::default();
    ferrum_server::app::App::register_tasks(&mut tasks);
    let names = tasks.names();
    assert!(names.iter().any(|n| n == "elsewhere_errand"), "its tasks: {names:?}");
    assert!(names.iter().any(|n| n == "create_tenant"), "beside the server's own");
}
