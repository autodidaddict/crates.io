// Iterates over every crate versions ever uploaded and (re-)renders their
// readme using the Markdown renderer from the cargo_registry crate.
//
// Warning: this can take a lot of time.
//
// Usage:
//     cargo run --bin render-readmes [page-size: optional = 25]
// The page-size argument dictate how much versions should be queried and processed at once.

#![deny(warnings)]

#[macro_use]
extern crate serde_derive;

extern crate cargo_registry;
extern crate curl;
extern crate diesel;
extern crate flate2;
extern crate s3;
extern crate tar;
extern crate time;
extern crate toml;
extern crate url;

use curl::easy::{Easy, List};
use diesel::prelude::*;
use flate2::read::GzDecoder;
use std::env;
use std::io::{Cursor, Read};
use std::path::Path;
use std::thread;
use tar::Archive;
use url::Url;

use cargo_registry::{Config, Version};
use cargo_registry::version::EncodableVersion;
use cargo_registry::schema::*;
use cargo_registry::render::markdown_to_html;

const DEFAULT_PAGE_SIZE: i64 = 25;

fn main() {
    let config: Config = Default::default();
    let conn = cargo_registry::db::connect_now().unwrap();
    let versions_count = versions::table
        .select(versions::all_columns)
        .count()
        .get_result::<i64>(&conn)
        .expect("error counting versions");
    let page_size = match env::args().nth(1) {
        None => DEFAULT_PAGE_SIZE,
        Some(s) => s.parse::<i64>().unwrap_or(DEFAULT_PAGE_SIZE),
    };
    let pages = if versions_count % page_size == 0 {
        versions_count / page_size
    } else {
        versions_count / page_size + 1
    };
    for current_page in 0..pages {
        let versions: Vec<EncodableVersion> = versions::table
            .inner_join(crates::table)
            .select((versions::all_columns, crates::name))
            .limit(page_size)
            .offset(current_page * page_size)
            .load::<(Version, String)>(&conn)
            .expect("error loading versions")
            .into_iter()
            .map(|(version, crate_name)| version.encodable(&crate_name))
            .collect();
        let mut tasks = Vec::with_capacity(page_size as usize);
        for version in versions {
            let config = config.clone();
            let handle = thread::spawn(move || {
                println!("[{}-{}] Rendering README...", version.krate, version.num);
                let readme = get_readme(&config, &version);
                if readme.is_none() {
                    return;
                }
                let readme = readme.unwrap();
                let readme_path = format!(
                    "readmes/{}/{}-{}.html",
                    version.krate,
                    version.krate,
                    version.num
                );
                let readme_len = readme.len();
                let mut body = Cursor::new(readme.into_bytes());
                config
                    .uploader
                    .upload(
                        Easy::new(),
                        &readme_path,
                        &mut body,
                        "text/html",
                        readme_len as u64,
                    )
                    .expect(&format!(
                        "[{}-{}] Couldn't upload file to S3",
                        version.krate,
                        version.num
                    ));
            });
            tasks.push(handle);
        }
        for handle in tasks {
            if let Err(err) = handle.join() {
                println!("Thead panicked: {:?}", err);
            }
        }
    }
}

/// Renders the readme of an uploaded crate version.
fn get_readme(config: &Config, version: &EncodableVersion) -> Option<String> {
    let mut handle = Easy::new();
    let location = match config.uploader.crate_location(&version.krate, &version.num) {
        Some(l) => l,
        None => return None,
    };
    let date = time::now().rfc822z().to_string();
    let url = Url::parse(&location).expect(&format!(
        "[{}-{}] Couldn't parse crate URL",
        version.krate,
        version.num
    ));

    let mut headers = List::new();
    headers
        .append(&format!("Host: {}", url.host().unwrap()))
        .unwrap();
    headers.append(&format!("Date: {}", date)).unwrap();

    handle.url(url.as_str()).unwrap();
    handle.get(true).unwrap();
    handle.http_headers(headers).unwrap();

    let mut response = Vec::new();
    {
        let mut req = handle.transfer();
        req.write_function(|data| {
            response.extend(data);
            Ok(data.len())
        }).unwrap();
        if let Err(err) = req.perform() {
            println!(
                "[{}-{}] Unable to fetch crate: {}",
                version.krate,
                version.num,
                err
            );
            return None;
        }
    }
    if handle.response_code().unwrap() != 200 {
        let response = String::from_utf8_lossy(&response);
        println!(
            "[{}-{}] Failed to get a 200 response: {}",
            version.krate,
            version.num,
            response
        );
        return None;
    }
    let reader = Cursor::new(response);
    let reader = GzDecoder::new(reader).expect(&format!(
        "[{}-{}] Invalid gzip header",
        version.krate,
        version.num
    ));
    let mut archive = Archive::new(reader);
    let mut entries = archive.entries().expect(&format!(
        "[{}-{}] Invalid tar archive entries",
        version.krate,
        version.num
    ));
    let manifest: Manifest = {
        let path = format!("{}-{}/Cargo.toml", version.krate, version.num);
        let contents = find_file_by_path(&mut entries, Path::new(&path), &version);
        toml::from_str(&contents).expect(&format!(
            "[{}-{}] Syntax error in manifest file",
            version.krate,
            version.num
        ))
    };
    if manifest.package.readme.is_none() {
        return None;
    }
    let rendered = {
        let path = format!(
            "{}-{}/{}",
            version.krate,
            version.num,
            manifest.package.readme.unwrap()
        );
        let contents = find_file_by_path(&mut entries, Path::new(&path), &version);
        markdown_to_html(&contents).expect(&format!(
            "[{}-{}] Couldn't render README",
            version.krate,
            version.num
        ))
    };
    return Some(rendered);
    #[derive(Deserialize)]
    struct Package {
        readme: Option<String>,
    }
    #[derive(Deserialize)]
    struct Manifest {
        package: Package,
    }
}

/// Search an entry by its path in a Tar archive.
fn find_file_by_path<R: Read>(
    mut entries: &mut tar::Entries<R>,
    path: &Path,
    version: &EncodableVersion,
) -> String {
    let mut file = entries
        .find(|entry| match *entry {
            Err(_) => false,
            Ok(ref file) => {
                let filepath = match file.path() {
                    Ok(p) => p,
                    Err(_) => return false,
                };
                return filepath == path;
            }
        })
        .expect(&format!(
            "[{}-{}] couldn't open file: {}",
            version.krate,
            version.num,
            path.display()
        ))
        .expect(&format!(
            "[{}-{}] file is not present: {}",
            version.krate,
            version.num,
            path.display()
        ));
    let mut contents = String::new();
    file.read_to_string(&mut contents).expect(&format!(
        "[{}-{}] Couldn't read file contents",
        version.krate,
        version.num
    ));
    contents
}
