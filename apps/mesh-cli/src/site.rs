//! `mesh site …`: publish a folder as a site, list and fetch sites, view one
//! in a browser through the safe local viewer (SPEC.md §10).

use super::{clean_metadata, coarsen, human_size, read_secret, save_direct, save_private, write_secret};
use anyhow::{bail, Context, Result};
use clap::Subcommand;
use mesh_crypto::Keypair;
use mesh_object::blob::guess_media_type;
use mesh_object::{create, Author};
use mesh_site::{Address, NewFile};
use mesh_store::{BlobSource, Store};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum SiteCmd {
    /// Make a key for a new site. The key is the site's address and lets you
    /// publish new versions; it is separate from your posting identity.
    New {
        /// A local name for the key (letters, digits, -).
        name: String,
    },
    /// Publish a folder as a site, or as a new version of one.
    Publish {
        /// Folder holding the site (hidden files are skipped).
        dir: PathBuf,
        /// The site key to sign with (from `mesh site new`).
        #[arg(long, conflicts_with = "ownerless", required_unless_present = "ownerless")]
        key: Option<String>,
        /// Publish with no key: anonymous, but it can never be updated.
        /// Always takes the private route.
        #[arg(long)]
        ownerless: bool,
        /// Title shown in site lists. Defaults to the previous version's.
        #[arg(long)]
        title: Option<String>,
        /// The first page.
        #[arg(long, default_value = "index.html")]
        index: String,
        /// Go public from a relay a few hops away, not from this device.
        #[arg(long)]
        private_route: bool,
        /// Publish files whose metadata can't be removed (e.g. SVG) as they are.
        #[arg(long)]
        keep_metadata: bool,
    },
    /// Sites this node holds (newest version of each).
    List,
    /// Queue a site's files for download; a running `mesh run` fetches them.
    Fetch { site: String },
    /// Show a site in your browser, through a local viewer that runs no
    /// scripts and loads nothing from outside the site.
    View {
        site: String,
        /// Local port (default: any free one).
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
}

fn key_path(home: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        bail!("site key names use letters, digits and - only");
    }
    Ok(home.join("sites").join(format!("{name}.key")))
}

/// Names of the site keys held here, with their addresses.
fn own_keys(home: &Path) -> Vec<(String, Address)> {
    let Ok(dir) = fs::read_dir(home.join("sites")) else { return vec![] };
    let mut out: Vec<(String, Address)> = dir
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().strip_suffix(".key")?.to_string();
            let seed = read_secret(&e.path()).ok()?;
            Some((name, Address::Keyed(Keypair::from_seed(&seed).public().id())))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub fn run(home: &Path, cmd: SiteCmd) -> Result<()> {
    match cmd {
        SiteCmd::New { name } => {
            let path = key_path(home, &name)?;
            if path.exists() {
                bail!("a site key called `{name}` already exists");
            }
            fs::create_dir_all(path.parent().unwrap())?;
            let k = Keypair::generate();
            write_secret(&path, &k.seed())?;
            println!("Site key `{name}`: {}", Address::Keyed(k.public().id()));
            println!("Publish a folder with: mesh site publish <folder> --key {name} --title \"…\"");
            eprintln!("note: anyone holding {} can publish versions of this site. Back it up; keep it private.", path.display());
            Ok(())
        }
        SiteCmd::Publish { dir, key, ownerless, title, index, private_route, keep_metadata } => {
            let store = Store::open(home)?;
            let site_key = match &key {
                Some(name) => {
                    let path = key_path(home, name)?;
                    let seed = read_secret(&path).with_context(|| format!("no site key `{name}`: make one with `mesh site new {name}`"))?;
                    Some(Keypair::from_seed(&seed))
                }
                None => None,
            };
            let previous = match &site_key {
                Some(k) => mesh_site::latest(&store, &Address::Keyed(k.public().id()))?,
                None => None,
            };
            let title = match (title, &previous) {
                (Some(t), _) => t,
                (None, Some(p)) => p.manifest.title.clone(),
                (None, None) => bail!("give the site a --title"),
            };
            let private = ownerless || private_route;
            let files = collect(&dir, keep_metadata)?;
            let total: u64 = files.iter().map(|f| f.data.len() as u64).sum();
            let limit = mesh_node::Config::default().max_anonymous_attachments;
            if private && total > limit {
                bail!(
                    "the site is {}; on the private route the limit is {} (every relay carries all of it). \
                     Publish it with a key from this device if being known as its source is acceptable.",
                    human_size(total),
                    human_size(limit)
                );
            }
            let source = if private { BlobSource::Cache } else { BlobSource::Own };
            let content = mesh_site::build(&store, files, &title, &index, previous.as_ref(), source)?;
            let reused = mesh_site::reused(&content, previous.as_ref());
            let n = content.attachments.len();
            let obj = match &site_key {
                Some(k) if private => create(coarsen(content), Author::Signed(k))?,
                Some(k) => create(content, Author::Signed(k))?,
                None => create(coarsen(content), Author::Ownerless)?,
            };
            let address = match &site_key {
                Some(k) => Address::Keyed(k.public().id()),
                None => Address::Ownerless(obj.id()),
            };
            let version = previous.map_or(1, |p| p.manifest.version + 1);
            println!("Site {address}");
            println!("  \"{title}\", version {version}: {n} files, {} ({reused} unchanged)", human_size(total));
            if ownerless {
                eprintln!("note: an ownerless site can never be updated or withdrawn; copies may persist indefinitely.");
            }
            if private {
                save_private(&store, &obj)
            } else {
                save_direct(&store, &obj)
            }
        }
        SiteCmd::List => {
            let store = Store::open(home)?;
            let mine = own_keys(home);
            let sites = mesh_site::sites(&store)?;
            if sites.is_empty() {
                println!("No sites yet. Publish one with `mesh site new` and `mesh site publish`.");
            }
            for s in &sites {
                let (have, all) = s.downloaded(&store)?;
                let whose = match mine.iter().find(|(_, a)| *a == s.address) {
                    Some((name, _)) => format!(" (yours: key `{name}`)"),
                    None if matches!(s.address, Address::Ownerless(_)) => " (ownerless)".into(),
                    None => String::new(),
                };
                println!("{}  {}{whose}", s.address.short(), s.manifest.title);
                println!(
                    "    version {}, {} files, {}, {have}/{all} downloaded",
                    s.manifest.version,
                    all,
                    human_size(s.total_size())
                );
            }
            for (name, address) in &mine {
                if !sites.iter().any(|s| s.address == *address) {
                    println!("{}  (key `{name}`, nothing published yet)", address.short());
                }
            }
            Ok(())
        }
        SiteCmd::Fetch { site } => {
            let store = Store::open(home)?;
            let s = mesh_site::find(&store, &site)?;
            mesh_site::want(&store, &s)?;
            let (have, all) = s.downloaded(&store)?;
            println!("Queued \"{}\": {have}/{all} files here so far.", s.manifest.title);
            eprintln!("(a running `mesh run` downloads them from connected peers)");
            Ok(())
        }
        SiteCmd::View { site, port } => {
            let store = Store::open(home)?;
            let s = mesh_site::find(&store, &site)?;
            mesh_site::want(&store, &s)?;
            let (have, all) = s.downloaded(&store)?;
            drop(store);
            let server = mesh_site::serve::Server::bind(home.to_path_buf(), s.address, port)?;
            println!("\"{}\" (version {}, {have}/{all} files here)", s.manifest.title, s.manifest.version);
            println!("Open in your browser:\n  {}", server.url());
            println!("Scripts don't run and nothing loads from outside the site. Links to other websites");
            println!("still leave SocialMesh if you click them. Press Ctrl+C to stop.");
            if have < all {
                eprintln!("note: some files are still downloading; keep `mesh run` running and refresh.");
            }
            server.run()
        }
    }
}

/// Every non-hidden file under `dir`, as site paths.
fn collect(dir: &Path, keep_metadata: bool) -> Result<Vec<NewFile>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).with_context(|| format!("reading {}", d.display()))? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = e.path();
            if e.file_type()?.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path.strip_prefix(dir)?;
            let site_path = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
            if !mesh_object::site::valid_path(&site_path) {
                bail!("`{site_path}`: site file names use letters, digits and . _ ~ - only (no spaces); rename it");
            }
            let media_type = guess_media_type(&name);
            let data = clean_metadata(&site_path, media_type, fs::read(&path)?, keep_metadata)?;
            out.push(NewFile { path: site_path, media_type: media_type.into(), data });
        }
    }
    if out.is_empty() {
        bail!("{} has no files", dir.display());
    }
    Ok(out)
}
