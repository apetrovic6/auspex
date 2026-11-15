//! The rendered service skeletons the api generators' tests drive.
//!
//! Lifted out of `database.rs`'s `mod tests`, where RT-31 wrote them, rather than copied into
//! each generator's tests: `new-feature`, `new-migration` and `new-domain` all need the same
//! rendered skeleton, and a skeleton copied four ways is a fixture that can drift from the
//! template it claims to be a copy of — with the drift showing up as a generator refusing for
//! a reason that has nothing to do with the defect under test.
//!
//! # Why a temporary tree at all
//!
//! `Service::open` resolves a name through `repo::root()`, which is `env!("CARGO_MANIFEST_DIR")`
//! baked in at compile time, so every name resolves inside this repository and no test can point
//! a generator at a tree it owns. Each generator's core takes the `&Service` instead — whose
//! `root`, `dir` and `name` are all public — and that is what makes a refusal testable: the test
//! hands the core a `Service` aimed at a `tempfile::tempdir()`, and compares the whole tree
//! against a snapshot taken before the call.
//!
//! # Why the fixtures chain
//!
//! Each generator's precondition is the previous generator's output: `new-migration` calls
//! `require_database()`, and `new-domain` needs a feature slice. Those preconditions are met by
//! running the generators that produce them, which is only possible because each core takes a
//! `&Service`. The chain is the same one `just check-template` walks, minus the
//! Postgres the probe cannot have.

use std::collections::BTreeMap;
use std::path::Path;

use super::{Service, database, feature};
use crate::{repo, scaffold};

/// The service skeleton, rendered into a tree this test owns, as `Service`.
///
/// Rendered **with a port**, which is what `just add service` does for every service. The value is
/// `scaffold::allocate_port`'s first service port, because this tree has no other service in it.
/// The no-port constructor used to be what this called, and it left `port = __port__` in the
/// fixture's `config/demo.toml` — not valid TOML, and the only reason nothing noticed is that
/// nothing used to read that file as TOML.
pub fn scaffolded_service(root: &Path) -> Service {
    let sentinels = scaffold::Sentinels::with_port("demo", 5000);
    copy_rendered(
        &repo::root().join("templates/service/services/__name__"),
        &root.join("services").join("demo"),
        &sentinels,
    );
    Service {
        root: root.to_path_buf(),
        dir: root.join("services").join("demo"),
        name: "demo".to_owned(),
    }
}

/// A service that has been through `add-database`, which is what `new-migration` refuses without.
pub fn service_with_database(root: &Path) -> Service {
    let service = scaffolded_service(root);
    database::convert(&service).expect("the skeleton this repository ships must convert");
    service
}

/// A service that has been through `add-database` and has its entity fixtures installed.
///
/// What `new-feature` needs before it is run: `require_database()` on one side, and the
/// `src/entities/<table>.rs` it reads its field list from on the other.
pub fn service_with_entities(root: &Path, feature_name: &str) -> Service {
    let service = service_with_database(root);
    install_fixture_entities(&service, feature_name);
    service
}

/// A service that has been through `add-database` and `new-feature`, which is what `new-domain`
/// refuses without.
///
/// `feature` is the feature `new-domain` is asked to add a DTO to, and it is generated here
/// rather than hand-copied because the file `new-domain` edits — `<feature>/domain/mod.rs` — is
/// `new-feature`'s output. A hand-written stand-in for it would be a fixture the generator was
/// tested against instead of the file it actually writes into.
pub fn service_with_feature(root: &Path, feature_name: &str) -> Service {
    let service = service_with_entities(root, feature_name);
    feature::scaffold_slice(&service, feature_name, None)
        .expect("a converted service with an entity must take a feature slice");
    service
}

/// Stand in for `just services/<svc>/entities` the way `check-template` does.
///
/// That recipe runs sea-orm-cli against a live schema, so it cannot run in a test either. The
/// committed fixture is copied over `src/entities/` instead — the same two files, for the same
/// table — which keeps `new-feature`'s field list coming from an entity of the right shape
/// rather than from one invented for the test.
///
/// The fixture is named for the table it describes, so the feature whose slice a test generates
/// has to be the one whose plural is that table name (`probe-lake` → `probe_lakes`).
fn install_fixture_entities(service: &Service, feature_name: &str) {
    let table = super::schema::pluralize(&feature_name.replace('-', "_"));
    let from = repo::root().join("tools/xtask/fixtures/entities");
    let to = service.src("entities");
    for entry in std::fs::read_dir(&from).expect("read the entity fixtures") {
        let entry = entry.expect("an entity fixture");
        if entry.path().extension().is_some_and(|ext| ext == "rs") {
            std::fs::copy(entry.path(), to.join(entry.file_name()))
                .expect("copy an entity fixture");
        }
    }
    assert!(
        service.entity(&table).is_some(),
        "the entity fixtures do not carry {table}.rs, so the feature name this test used has \
         no fixture entity: {feature_name}"
    );
}

/// Copy a template tree, substituting the scaffold sentinels in path components as well as
/// in file contents.
///
/// Rendered rather than copied raw because the anchors are written against what
/// `just add service` produces: a `__snake__` left in `helpers.rs` would make a planner refuse
/// for a reason that has nothing to do with the test, and the failure would look like the defect
/// while being nothing of the kind.
pub fn copy_rendered(from: &Path, to: &Path, sentinels: &scaffold::Sentinels) {
    std::fs::create_dir_all(to).expect("create the copy of the skeleton");
    for entry in std::fs::read_dir(from).expect("read the skeleton") {
        let entry = entry.expect("a skeleton entry");
        let target = to.join(sentinels.apply(&entry.file_name().to_string_lossy()));
        if entry.path().is_dir() {
            copy_rendered(&entry.path(), &target, sentinels);
        } else {
            let text = std::fs::read_to_string(entry.path()).expect("read a skeleton file");
            std::fs::write(&target, sentinels.apply(&text)).expect("write the copy");
        }
    }
}

/// Every file under `dir`, as path → contents, for comparing a refused generation against the
/// tree it was handed.
///
/// Keyed by the full path rather than a relative one because the tree is a temporary directory
/// whose name is not known in advance; the paths differ between the snapshot and the comparison
/// only if the tree itself moved, which is not a thing these tests do.
pub fn tree(dir: &Path) -> BTreeMap<String, String> {
    let mut files = BTreeMap::new();
    let mut walk = vec![dir.to_path_buf()];
    while let Some(dir) = walk.pop() {
        for entry in std::fs::read_dir(&dir).expect("read the tree") {
            let entry = entry.expect("a tree entry");
            let path = entry.path();
            if path.is_dir() {
                walk.push(path);
            } else {
                files.insert(
                    path.to_string_lossy().into_owned(),
                    std::fs::read_to_string(&path).expect("read a file in the tree"),
                );
            }
        }
    }
    files
}
