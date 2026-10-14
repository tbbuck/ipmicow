//! BMC web-UI plumbing: log in and mint a fresh iKVM console token.
//!
//! The ATEN viewer's token is ephemeral and tied to a web-login session, so the
//! client has to perform the login + JNLP mint itself rather than reuse a stale
//! `.jnlp`. This module replicates exactly what the browser's "Launch Console"
//! button does (`deployJava.launch(url_redirect.cgi?url_name=ikvm&url_type=jwsk)`).

use anyhow::{anyhow, bail, Context, Result};
use std::time::Duration;

/// Everything needed to open an iKVM session, recovered from a freshly minted JNLP.
#[derive(Debug, Clone)]
#[allow(dead_code)] // ipmi_port / raw_args are parsed but not yet consumed
pub struct ConsoleParams {
    pub host: String,
    pub token: String,
    pub video_port: u16,
    pub ipmi_port: u16,
    pub raw_args: Vec<String>,
}

/// Log in, mint a fresh JNLP, parse the console params, then free the session.
pub fn mint_console(base_url: &str, user: &str, pass: &str) -> Result<ConsoleParams> {
    let client = reqwest::blocking::Client::builder()
        .danger_accept_invalid_certs(true) // BMC ships a self-signed cert
        .danger_accept_invalid_hostnames(true)
        .timeout(Duration::from_secs(10))
        .build()
        .context("building HTTP client")?;

    let sid = login(&client, base_url, user, pass)?;
    let jnlp = mint_jnlp(&client, base_url, &sid)?;
    let params = parse_jnlp(&jnlp)?;

    // Best-effort: the X9 BMC only allows ~2 concurrent web sessions, so don't squat.
    let _ = logout(&client, base_url, &sid);

    Ok(params)
}

fn login(client: &reqwest::blocking::Client, base: &str, user: &str, pass: &str) -> Result<String> {
    let resp = client
        .post(format!("{base}/cgi/login.cgi"))
        .form(&[("name", user), ("pwd", pass)])
        .send()
        .context("POST /cgi/login.cgi")?;

    // Login sends two `Set-Cookie: SID=...` headers — first clears it (empty value),
    // second sets the real session id. Keep the last non-empty one.
    let mut sid = None;
    for hv in resp.headers().get_all(reqwest::header::SET_COOKIE) {
        if let Ok(s) = hv.to_str() {
            if let Some(rest) = s.strip_prefix("SID=") {
                let val = rest.split(';').next().unwrap_or("").trim();
                if !val.is_empty() {
                    sid = Some(val.to_string());
                }
            }
        }
    }

    sid.ok_or_else(|| {
        anyhow!("login returned no SID — bad credentials, or the BMC's 2 web sessions are already in use")
    })
}

fn mint_jnlp(client: &reqwest::blocking::Client, base: &str, sid: &str) -> Result<String> {
    // The jwsk handler 500s without the browser's client-side `language` cookie and a
    // referer, so we send exactly what Safari/Chrome send.
    let resp = client
        .get(format!("{base}/cgi/url_redirect.cgi?url_name=ikvm&url_type=jwsk"))
        .header("Cookie", format!("langSetFlag=0; language=English; SID={sid}"))
        .header("Referer", format!("{base}/cgi/url_redirect.cgi?url_name=man_ikvm"))
        .send()
        .context("GET iKVM JNLP")?;

    let status = resp.status();
    let body = resp.text().context("reading JNLP body")?;
    if !status.is_success() || !body.contains("<jnlp") {
        let head: String = body.chars().take(160).collect();
        bail!("did not get a JNLP (HTTP {status}); first bytes: {head:?}");
    }
    Ok(body)
}

fn logout(client: &reqwest::blocking::Client, base: &str, sid: &str) -> Result<()> {
    client
        .get(format!("{base}/cgi/logout.cgi"))
        .header("Cookie", format!("SID={sid}"))
        .send()
        .context("GET /cgi/logout.cgi")?;
    Ok(())
}

/// Extract the `<argument>` values from the JNLP `application-desc`.
///
/// Observed layout: [host, token, token, null, video_port, ipmi_port, flag, flag]
fn parse_jnlp(jnlp: &str) -> Result<ConsoleParams> {
    const OPEN: &str = "<argument>";
    const CLOSE: &str = "</argument>";

    let args: Vec<String> = jnlp
        .match_indices(OPEN)
        .filter_map(|(i, _)| {
            let start = i + OPEN.len();
            let end = jnlp[start..].find(CLOSE)? + start;
            Some(jnlp[start..end].trim().to_string())
        })
        .collect();

    if args.len() < 6 {
        bail!("unexpected JNLP: found {} <argument>s, expected >= 6", args.len());
    }

    let video_port = args[4].parse().context("parsing video port (arg 4)")?;
    let ipmi_port = args[5].parse().context("parsing ipmi port (arg 5)")?;

    Ok(ConsoleParams { host: args[0].clone(), token: args[1].clone(), video_port, ipmi_port, raw_args: args })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Sanity-check the JNLP parser against the shape of a real minted JNLP.
    #[test]
    fn parses_observed_jnlp() {
        let jnlp = r#"
          <application-desc main-class="tw.com.aten.ikvm.KVMMain">
            <argument>192.0.2.10</argument>
            <argument>abcdefghijklmnop</argument>
            <argument>abcdefghijklmnop</argument>
            <argument>null</argument>
            <argument>5900</argument>
            <argument>623</argument>
            <argument>2</argument>
            <argument>0</argument>
          </application-desc>"#;
        let p = parse_jnlp(jnlp).unwrap();
        assert_eq!(p.host, "192.0.2.10");
        assert_eq!(p.token, "abcdefghijklmnop");
        assert_eq!(p.video_port, 5900);
        assert_eq!(p.ipmi_port, 623);
        assert_eq!(p.raw_args.len(), 8);
    }
}
