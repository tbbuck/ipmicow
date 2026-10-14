//! IMM2 web-UI plumbing: log in and mint a single-use KVM token.

use std::time::Duration;

use anyhow::{anyhow, Context};

/// Mint a fresh single-use console token from the IMM2 web UI: log in, then GET
/// the dynamically-generated JNLP and pull `user=0x…` out of it.
pub fn mint_imm_token(host: &str, user: &str, pass: &str) -> anyhow::Result<String> {
    let client = reqwest::blocking::Client::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .timeout(Duration::from_secs(15))
        .build()?;
    let base = format!("https://{host}");

    // 1. Log in; capture the appweb session cookie.
    let resp = client
        .post(format!("{base}/data/login"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("user={user}&password={pass}"))
        .send()
        .context("POST /data/login")?;
    let mut sid = None;
    for hv in resp.headers().get_all(reqwest::header::SET_COOKIE) {
        if let Ok(s) = hv.to_str() {
            if let Some(rest) = s.strip_prefix("_appwebSessionId_=") {
                sid = Some(rest.split(';').next().unwrap_or("").to_string());
            }
        }
    }
    let sid = sid.ok_or_else(|| anyhow!("login set no _appwebSessionId_ cookie (bad creds?)"))?;

    // 2. Mint the JNLP with the launch string the web UI builds
    //    (serverRemoteControl.js `_20`/`_23`):
    //      ip @ webPort @ ipFormat @ timestampMs @ encrypt @ exclusive @ platform
    //         @ jnlp @ user @ knock @ preemptTimeout @ reserveId @ authKey @ javaSec
    //    A short string still yields a KVM token, but "result: fail" and `vm=0`:
    //    the session then has no virtual-media binding (appliance random 0, and
    //    the VM server refuses it).
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let ls = format!("{host}@443@0@{ts}@0@1@1@jnlp@{user}@0@0@0@0@1");
    let body = client
        .get(format!("{base}/designs/imm/viewer({ls}).jnlp"))
        .header("Cookie", format!("_appwebSessionId_={sid}"))
        .send()
        .context("GET viewer JNLP")?
        .text()?;

    let token = body
        .find("user=0x")
        .map(|p| body[p + 5..].chars().take_while(|c| c.is_ascii_hexdigit() || *c == 'x').collect::<String>())
        .filter(|t| t.len() >= 4)
        .ok_or_else(|| anyhow!("no user=0x token in JNLP response"))?;
    Ok(token)
}
