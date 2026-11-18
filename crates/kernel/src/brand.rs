//! What the workspace looks like to people outside it (docs/builder/decisions.md BD-79): the `core` package's Brand
//! records, read by every public page the platform or an app draws. A page shows the brand an app's record names, else
//! its host's (`tenant_host.brand`), else the workspace's default, else the platform's own look.
//!
//! A brand sets only the accent and the typeface: the page and surface colours stay the platform's tokens, and the
//! accent is nudged until it reads at 4.5:1 on each of them in light and dark ([`tokens`]), so no combination can fail.
//! Its stylesheet is written only from `#rrggbb` values and fixed font stacks: no record text reaches it. Its files
//! (logo, favicon, typeface) are files uploaded for the brand, served to strangers only through the brand's own route
//! ([`Kernel::brand_file`]) with the type their bytes show. Beside it, who may frame a public page ([`Kernel::embedders`],
//! BD-80).

use crate::{Error, Kernel, Result};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

/// The platform's page and surface colours a brand's accent must read on (server/shell/theme.css `--ferrum-bg`,
/// `--ferrum-surface`), light then dark.
const LIGHT_SURFACES: [&str; 2] = ["#ffffff", "#f6f7f9"];
const DARK_SURFACES: [&str; 2] = ["#111317", "#191c22"];
/// Text on the accent: white in light mode, the dark page colour in dark mode (each passes by construction).
const TEXT_LIGHT: &str = "#ffffff";
const TEXT_DARK: &str = "#111317";
/// The largest brand file served (a logo, a favicon, a typeface).
pub const FILE_MAX: i64 = 2 << 20;
/// Links under a public page.
pub const LINKS_MAX: usize = 8;

/// A brand's accent as the colours pages paint with (`#rrggbb` each).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokens {
    pub light: String,
    pub dark: String,
    pub hover_light: String,
    pub hover_dark: String,
    pub soft_light: String,
    pub soft_dark: String,
}

/// `#rgb` or `#rrggbb` (any case) as `#rrggbb`; `None` for anything else.
pub fn hex(value: &str) -> Option<String> {
    let v = value.trim().to_ascii_lowercase();
    let digits = v.strip_prefix('#')?;
    if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    match digits.len() {
        6 => Some(v),
        3 => Some(digits.chars().fold(String::from("#"), |mut s, c| {
            s.push(c);
            s.push(c);
            s
        })),
        _ => None,
    }
}

fn rgb(hex: &str) -> [f64; 3] {
    [1, 3, 5].map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0) as f64)
}

fn mix(a: &str, b: &str, t: f64) -> String {
    let (a, b) = (rgb(a), rgb(b));
    let c = [0, 1, 2].map(|i| (a[i] + (b[i] - a[i]) * t).clamp(0.0, 255.0).round() as u8);
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

fn luminance(hex: &str) -> f64 {
    let [r, g, b] = rgb(hex).map(|c| {
        let s = c / 255.0;
        if s <= 0.03928 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
    });
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// The WCAG contrast ratio of two `#rrggbb` colours, 1 to 21.
pub fn contrast(a: &str, b: &str) -> f64 {
    let (x, y) = (luminance(a), luminance(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// The first of `colour` moved toward `target` in 25 even steps that reads at 4.5:1 on every surface; `target` if none.
fn until(colour: &str, target: &str, surfaces: &[&str]) -> String {
    (0..=25).map(|i| mix(colour, target, i as f64 / 25.0)).find(|c| surfaces.iter().all(|s| contrast(c, s) >= 4.5)).unwrap_or_else(|| target.into())
}

/// The accent `primary` (and its own dark-mode colour, if given) as it is shown: each kept where it reads at 4.5:1 on
/// the platform's surfaces, else darkened (light mode) or lightened (dark mode) step by step until it does. Pure; the
/// interface kit's `accentTokens` gives the same values (Ferrum-Frontend `src/lib/theme.ts`). Invalid input is read as
/// the platform's own accent.
pub fn tokens(primary: &str, primary_dark: Option<&str>) -> Tokens {
    let primary = hex(primary).unwrap_or_else(|| "#3157d5".into());
    let dark_base = primary_dark.and_then(hex).unwrap_or_else(|| primary.clone());
    let light = until(&primary, "#000000", &LIGHT_SURFACES);
    let dark = until(&dark_base, "#ffffff", &DARK_SURFACES);
    Tokens {
        hover_light: mix(&light, "#000000", 0.15),
        hover_dark: mix(&dark, "#ffffff", 0.15),
        soft_light: mix(&light, "#ffffff", 0.88),
        soft_dark: mix(&dark, DARK_SURFACES[1], 0.78),
        light,
        dark,
    }
}

/// A link a public page may carry: `https://…`, `mailto:…`, or a path of the page's own host (`/privacy`, never
/// `//elsewhere`); no whitespace or control characters anywhere.
pub fn safe_link(url: &str) -> bool {
    !url.is_empty()
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
        && (url.starts_with("https://") || url.starts_with("mailto:") || (url.starts_with('/') && !url.starts_with("//")))
}

/// The CSS font stack of a brand's typeface choice; `own`: whether it has its own typeface file.
fn font_stack(font: &str, own: bool) -> &'static str {
    match font {
        "Own" if own => r#""ferrum-brand", system-ui, sans-serif"#,
        "Serif" => r#"Georgia, Cambria, "Times New Roman", serif"#,
        "Rounded" => r#"ui-rounded, "SF Pro Rounded", system-ui, sans-serif"#,
        "Mono" => "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
        _ => r#"system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif"#,
    }
}

/// The brand's files a stranger may read, the field each is kept in, and the types its bytes may show.
pub const FILE_PARTS: [(&str, &[&str]); 4] = [
    ("logo", &["image/png", "image/jpeg", "image/webp"]),
    ("logo_dark", &["image/png", "image/jpeg", "image/webp"]),
    ("favicon", &["image/png"]),
    ("font_file", &["font/woff2"]),
];

/// The type a file's first bytes show, of those a brand serves; `None` for anything else (SVG, HTML, a renamed file).
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, ..] => Some("image/png"),
        [0xff, 0xd8, 0xff, ..] => Some("image/jpeg"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        [b'w', b'O', b'F', b'2', ..] => Some("font/woff2"),
        _ => None,
    }
}

/// A brand as a public page shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct PublicBrand {
    pub id: Uuid,
    pub name: String,
    /// 12 hex digits of when the record last changed: its stylesheet's and files' addresses change with it.
    pub version: String,
    /// Which of its files it has (`logo`, `logo_dark`, `favicon`, `font_file`).
    pub files: Vec<&'static str>,
    pub footer_text: String,
    /// `(label, url)`, each [`safe_link`].
    pub links: Vec<(String, String)>,
    pub tokens: Tokens,
    pub font: String,
}

impl PublicBrand {
    /// Where one of its files is served: `/_brand/{slug}/{id}/{part}?v={version}`; `None` when it has none.
    pub fn file_href(&self, slug: &str, part: &str) -> Option<String> {
        self.files.contains(&part).then(|| format!("/_brand/{slug}/{}/{part}?v={}", self.id, self.version))
    }

    /// `/_brand/{slug}/{id}.css?v={version}`.
    pub fn css_href(&self, slug: &str) -> String {
        format!("/_brand/{slug}/{}.css?v={}", self.id, self.version)
    }

    /// What a page links in its `<head>`: the stylesheet, and the favicon when the brand has one. Escaped.
    pub fn head(&self, slug: &str) -> String {
        let mut out = format!(r#"<link rel="stylesheet" href="{}">"#, crate::layout::esc(&self.css_href(slug)));
        if let Some(icon) = self.file_href(slug, "favicon") {
            out.push_str(&format!(r#"<link rel="icon" href="{}" type="image/png">"#, crate::layout::esc(&icon)));
        }
        out
    }

    /// Its stylesheet: the accent and typeface as both token families (the platform's `--ferrum-*`, server/shell/theme.css,
    /// and the interface kit's), and its own typeface when it has one. Only validated colours and fixed text.
    pub fn css(&self, slug: &str) -> String {
        let t = &self.tokens;
        let ld = |a: &str, b: &str| format!("light-dark({a}, {b})");
        let ring = {
            let ([lr, lg, lb], [dr, dg, db]) = (rgb(&t.light), rgb(&t.dark));
            format!("0 0 0 3px light-dark(rgb({lr} {lg} {lb} / 0.75), rgb({dr} {dg} {db} / 0.8))")
        };
        let own = self.files.contains(&"font_file");
        let vars = [
            ("primary", ld(&t.light, &t.dark)),
            ("primary-hover", ld(&t.hover_light, &t.hover_dark)),
            ("primary-soft", ld(&t.soft_light, &t.soft_dark)),
            ("primary-text", ld(TEXT_LIGHT, TEXT_DARK)),
            ("focus-ring", ring),
        ];
        let mut css = String::from(":root{");
        for (name, value) in &vars {
            css.push_str(&format!("--ferrum-{name}:{value};--{name}:{value};"));
        }
        let stack = font_stack(&self.font, own);
        css.push_str(&format!("--ferrum-font:{stack};--font:{stack};}}"));
        if own && self.font == "Own" {
            css.push_str(&format!(
                r#"@font-face{{font-family:"ferrum-brand";src:url("/_brand/{slug}/{}/font_file?v={}") format("woff2");font-display:swap}}"#,
                self.id, self.version
            ));
        }
        css
    }

    /// What a public or portal page's bootstrap carries (`brand`): its name, files' addresses, footer and links, and the
    /// stylesheet the shell links.
    pub fn boot(&self, slug: &str) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "css": self.css_href(slug),
            "logo": self.file_href(slug, "logo"),
            "logo_dark": self.file_href(slug, "logo_dark").or_else(|| self.file_href(slug, "logo")),
            "favicon": self.file_href(slug, "favicon"),
            "footer_text": self.footer_text,
            "links": self.links.iter().map(|(label, url)| json!({"label": label, "url": url})).collect::<Vec<_>>(),
        })
    }
}

/// The stored file an `attach` value names (`/files/{id}`).
pub fn file_id(value: Option<&str>) -> Option<Uuid> {
    value?.strip_prefix("/files/")?.parse().ok()
}

impl Kernel {
    /// The brand a public page of `tenant` shows: `named` (an app record's), else `host`'s (`tenant_host.brand`), else the
    /// workspace's default; `None`: the platform's own look (no Brand record, or a workspace whose packages have none).
    pub async fn public_brand(&self, tenant: Uuid, named: Option<Uuid>, host: Option<Uuid>) -> Result<Option<PublicBrand>> {
        if self.registry.schema(tenant).await?.entity("Brand").is_none() {
            return Ok(None);
        }
        let mut tx = self.db.tenant_tx(tenant).await?;
        let mut found = None;
        for id in [named, host].into_iter().flatten() {
            found = brand_row(&mut tx, tenant, Some(id)).await?;
            if found.is_some() {
                break;
            }
        }
        if found.is_none() {
            found = brand_row(&mut tx, tenant, None).await?;
        }
        let Some(row) = found else {
            tx.commit().await?;
            return Ok(None);
        };
        let links: Vec<(String, String)> =
            sqlx::query_as("select label, url from e_brand__links where tenant_id = $1 and parent_id = $2 order by idx limit $3")
                .bind(tenant)
                .bind(row.id)
                .bind(LINKS_MAX as i64)
                .fetch_all(&mut *tx)
                .await?;
        tx.commit().await?;
        let files = FILE_PARTS
            .iter()
            .map(|(part, _)| *part)
            .zip([&row.logo, &row.logo_dark, &row.favicon, &row.font_file])
            .filter(|(_, v)| file_id(v.as_deref()).is_some())
            .map(|(part, _)| part)
            .collect();
        Ok(Some(PublicBrand {
            id: row.id,
            name: row.brand_name,
            version: format!("{:012x}", row.updated_at.timestamp_micros() as u64 & 0xffff_ffff_ffff),
            files,
            footer_text: row.footer_text.unwrap_or_default(),
            links: links.into_iter().filter(|(_, url)| safe_link(url)).collect(),
            tokens: tokens(&row.primary_color, row.primary_color_dark.as_deref()),
            font: row.font.unwrap_or_default(),
        }))
    }

    /// Brand `brand` of `tenant`, exactly (the brand route): `None` when it is not one of the workspace's.
    pub async fn brand_by_id(&self, tenant: Uuid, brand: Uuid) -> Result<Option<PublicBrand>> {
        Ok(self.public_brand(tenant, Some(brand), None).await?.filter(|b| b.id == brand))
    }

    /// One of brand `brand`'s files (`part`, of [`FILE_PARTS`]), for anyone: its type as its bytes show it, and the
    /// bytes. `None` (not found, alike) unless the brand is the workspace's, names the file in that field, the file was
    /// uploaded for this brand, is at most [`FILE_MAX`] and its bytes are of a type the field takes.
    pub async fn brand_file(&self, tenant: Uuid, brand: Uuid, part: &str) -> Result<Option<(&'static str, Vec<u8>)>> {
        let Some((_, types)) = FILE_PARTS.iter().find(|(p, _)| *p == part) else { return Ok(None) };
        if self.registry.schema(tenant).await?.entity("Brand").is_none() {
            return Ok(None);
        }
        let mut tx = self.db.tenant_tx(tenant).await?;
        // `part` is one of FILE_PARTS' names, each a column of e_brand.
        let named: Option<Option<String>> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select {part} from e_brand where tenant_id = $1 and id = $2")))
            .bind(tenant)
            .bind(brand)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(file) = file_id(named.flatten().as_deref()) else {
            tx.commit().await?;
            return Ok(None);
        };
        let key: Option<String> = sqlx::query_scalar(
            "select storage_key from file_object where tenant_id = $1 and id = $2 and entity = 'Brand' and record_id = $3 and size_bytes <= $4",
        )
        .bind(tenant)
        .bind(file)
        .bind(brand)
        .bind(FILE_MAX)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        let Some(key) = key else { return Ok(None) };
        let bytes = self.file_store()?.get(&key).await?;
        Ok(sniff(&bytes).filter(|t| types.contains(t)).map(|t| (t, bytes)))
    }

    /// Sets the brand address `host` of the caller's workspace shows (`None`: the default one). Administrators only; not
    /// for a `mail` address; the brand must be a Brand record of the workspace, checked in the same transaction. This
    /// replica shows it at once, the others within the host cache's 20 seconds.
    pub async fn set_host_brand(&self, ctx: &crate::Ctx, host: &str, brand: Option<Uuid>) -> Result<()> {
        crate::shell::may_change_addresses(ctx)?;
        let host = crate::shell::normalize_host(host).ok_or_else(|| Error::NotFound("no such domain".into()))?;
        // tenant_host is a cluster table; every statement here names the workspace.
        let mut tx = self.db.system_tenant_tx(ctx.tenant).await?;
        if let Some(b) = brand {
            let known = ctx.schema.entity("Brand").is_some()
                && sqlx::query_scalar::<_, i32>("select 1 from e_brand where tenant_id = $1 and id = $2")
                    .bind(ctx.tenant)
                    .bind(b)
                    .fetch_optional(&mut *tx)
                    .await?
                    .is_some();
            if !known {
                return Err(Error::field("brand", "is not a brand of this workspace"));
            }
        }
        let target: Option<String> = sqlx::query_scalar("select target from tenant_host where host = $1 and tenant_id = $2 for update")
            .bind(&host)
            .bind(ctx.tenant)
            .fetch_optional(&mut *tx)
            .await?;
        match target.as_deref() {
            None => return Err(Error::NotFound("no such domain".into())),
            Some("mail") => return Err(Error::field("brand", "a mail address shows no pages")),
            Some(_) => {}
        }
        sqlx::query("update tenant_host set brand = $3 where host = $1 and tenant_id = $2").bind(&host).bind(ctx.tenant).bind(brand).execute(&mut *tx).await?;
        crate::audit::record_admin(&mut tx, ctx, "WorkspaceDomain", ctx.tenant, "brand", None, json!({"host": host, "brand": brand})).await?;
        tx.commit().await?;
        self.hosts.remove(&host);
        Ok(())
    }
}

impl Kernel {
    /// The origins of the Embedding of `tenant` whose page is `path` or its longest start at a `/`
    /// (docs/builder/decisions.md BD-80): the outside sites that may frame that public page. Compared as text, so no
    /// character of a page is a wildcard. None when there is none, or the workspace's packages have no Embedding.
    pub async fn embedders(&self, tenant: Uuid, path: &str) -> Result<Vec<String>> {
        if self.registry.schema(tenant).await?.entity("Embedding").is_none() {
            return Ok(vec![]);
        }
        let path = path.trim_matches('/');
        let mut tx = self.db.tenant_tx(tenant).await?;
        let origins: Option<String> = sqlx::query_scalar(
            "select origins from e_embedding where tenant_id = $1 and ($2 = page or left($2, length(page) + 1) = page || '/') \
             order by length(page) desc limit 1",
        )
        .bind(tenant)
        .bind(path)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        // Each was checked when saved; checked again, so a row written otherwise names nothing that is not an origin.
        Ok(origins.unwrap_or_default().lines().filter_map(ferrum_meta::published::frame_origin).take(10).collect())
    }

    /// The live addresses of `tenant` that serve pages (every target but `mail`): proven or under the base domain, not
    /// released, not forwarding. Its own public pages may be framed by each.
    pub async fn page_hosts(&self, tenant: Uuid) -> Result<Vec<String>> {
        let mut tx = self.db.system_tx().await?;
        let hosts = sqlx::query_scalar(
            "select host from tenant_host where tenant_id = $1 and target <> 'mail' and released_at is null \
             and (kind = 'subdomain' or verified_at is not null) and not forward order by host",
        )
        .bind(tenant)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(hosts)
    }
}

#[derive(sqlx::FromRow)]
struct BrandRow {
    id: Uuid,
    brand_name: String,
    updated_at: DateTime<Utc>,
    primary_color: String,
    primary_color_dark: Option<String>,
    logo: Option<String>,
    logo_dark: Option<String>,
    favicon: Option<String>,
    font: Option<String>,
    font_file: Option<String>,
    footer_text: Option<String>,
}

/// Brand `id` of `tenant`, or (`None`) its default: one at most is, and a race that slipped past still shows one.
async fn brand_row(tx: &mut crate::Tx, tenant: Uuid, id: Option<Uuid>) -> Result<Option<BrandRow>> {
    Ok(sqlx::query_as(
        "select id, brand_name, updated_at, primary_color, primary_color_dark, logo, logo_dark, favicon, font, font_file, footer_text \
         from e_brand where tenant_id = $1 and (id = $2 or ($2 is null and is_default)) order by created_at, id limit 1",
    )
    .bind(tenant)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Twenty colours, the hard ones among them: pure yellow, mid grey, black, white, near-white, pastels.
    const COLOURS: [&str; 20] = [
        "#ffff00", "#777777", "#000000", "#ffffff", "#3157d5", "#e11d48", "#00ff00", "#00ffff", "#ff00ff", "#0000ff", "#ff0000", "#f6f7f9", "#111317",
        "#191c22", "#ffa500", "#808000", "#4b0082", "#fafad2", "#1e1e1e", "#9cb3ff",
    ];

    #[test]
    fn tokens_pass_aa_in_light_and_dark() {
        for c in COLOURS {
            let t = tokens(c, None);
            for (shown, hover, surfaces, text) in [(&t.light, &t.hover_light, LIGHT_SURFACES, TEXT_LIGHT), (&t.dark, &t.hover_dark, DARK_SURFACES, TEXT_DARK)] {
                for s in surfaces {
                    assert!(contrast(shown, s) >= 4.5, "{c}: {shown} on {s} is {:.2}", contrast(shown, s));
                    assert!(contrast(hover, s) >= 4.5, "{c}: hover {hover} on {s} is {:.2}", contrast(hover, s));
                }
                assert!(contrast(shown, text) >= 4.5, "{c}: text {text} on {shown} is {:.2}", contrast(shown, text));
            }
        }
        // One that passes is shown as given; one given in short form or capitals is the same colour.
        assert_eq!(tokens("#3157D5", None).light, "#3157d5");
        assert_eq!(tokens("#000", None), tokens("#000000", None));
        // Its own dark-mode colour is used where it passes.
        assert_eq!(tokens("#000000", Some("#9cb3ff")).dark, "#9cb3ff");
    }

    /// The vectors the interface kit's `accentTokens` is checked against (Ferrum-Frontend `src/lib/theme.test.ts`):
    /// colour, dark-mode colour, then light, dark, hover light, hover dark, soft light, soft dark.
    pub const VECTORS: [(&str, Option<&str>, [&str; 6]); 5] = [
        ("#3157d5", None, ["#3157d5", "#627fdf", "#2a4ab5", "#7a92e4", "#e6ebfa", "#29324c"]),
        ("#ffff00", None, ["#707000", "#ffff00", "#5f5f00", "#ffff26", "#eeeee0", "#4c4e1b"]),
        ("#777777", None, ["#6d6d6d", "#878787", "#5d5d5d", "#999999", "#ededed", "#313438"]),
        ("#000000", None, ["#000000", "#858585", "#000000", "#979797", "#e0e0e0", "#313338"]),
        ("#e11d48", Some("#9f1239"), ["#d81c45", "#c26780", "#b8183b", "#cb7e93", "#fae4e9", "#3e2d37"]),
    ];

    #[test]
    fn tokens_match_forms_accent_vectors() {
        for (c, d, [light, dark, hover_light, hover_dark, soft_light, soft_dark]) in VECTORS {
            let want = Tokens {
                light: light.into(),
                dark: dark.into(),
                hover_light: hover_light.into(),
                hover_dark: hover_dark.into(),
                soft_light: soft_light.into(),
                soft_dark: soft_dark.into(),
            };
            assert_eq!(tokens(c, d), want, "{c} {d:?}");
        }
    }

    #[test]
    fn links_and_files_keep_to_their_forms() {
        for ok in ["https://acme.example/privacy", "mailto:help@acme.example", "/terms"] {
            assert!(safe_link(ok), "{ok}");
        }
        for bad in ["http://acme.example", "//evil.example", "javascript:alert(1)", "https://a b", "/x\n", "", "data:text/html,x"] {
            assert!(!safe_link(bad), "{bad}");
        }
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"wOF2\0\x01"), Some("font/woff2"));
        assert_eq!(sniff(b"<svg onload=alert(1)>"), None);
        assert_eq!(sniff(b"<!doctype html>"), None);
        assert_eq!(hex("#ABC").as_deref(), Some("#aabbcc"));
        assert_eq!(hex("red"), None);
        assert_eq!(
            file_id(Some("/files/0190a0a0-0000-7000-8000-000000000001")).map(|u| u.to_string()).as_deref(),
            Some("0190a0a0-0000-7000-8000-000000000001")
        );
        assert_eq!(file_id(Some("https://evil.example/x.png")), None);
    }

    #[test]
    fn its_stylesheet_holds_only_colours_and_fixed_text() {
        let b = PublicBrand {
            id: Uuid::nil(),
            name: "</style><script>".into(),
            version: "0123456789ab".into(),
            files: vec!["favicon", "font_file"],
            footer_text: "url(javascript:x) @import".into(),
            links: vec![],
            tokens: tokens("#ffff00", None),
            font: "Own".into(),
        };
        let css = b.css("acme");
        assert!(css.contains("--ferrum-primary:light-dark(#707000, #ffff00);--primary:light-dark(#707000, #ffff00);"), "{css}");
        assert!(css.contains("--font:\"ferrum-brand\""), "{css}");
        for bad in ["</style", "<script", "@import", "javascript"] {
            assert!(!css.contains(bad), "{bad} in {css}");
        }
        // The only url() is its own typeface's route.
        assert_eq!(css.matches("url(").count(), 1);
        assert!(css.contains(r#"url("/_brand/acme/00000000-0000-0000-0000-000000000000/font_file?v=0123456789ab")"#));
        let head = b.head("acme");
        assert_eq!(
            head,
            r#"<link rel="stylesheet" href="/_brand/acme/00000000-0000-0000-0000-000000000000.css?v=0123456789ab"><link rel="icon" href="/_brand/acme/00000000-0000-0000-0000-000000000000/favicon?v=0123456789ab" type="image/png">"#
        );
        assert_eq!(b.boot("acme")["logo"], Value::Null);
    }
}
