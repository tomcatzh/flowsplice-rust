use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::{
    digest,
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_ASN1, ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair},
};
use rustls_pki_types::PrivateKeyDer;
use serde::{Deserialize, Serialize};

use crate::protocol::Role;

pub const STATISTICS_REPORT_VERSION: u32 = 1;

/// Produces the small dependency-free local statistics dashboard shared by headless roles.
///
/// The page reads only the same-origin loopback JSON API and never renders raw JSON as the
/// reporting product. All values inserted by JavaScript are HTML-escaped first.
#[must_use]
pub fn statistics_dashboard_html(title: &str, subtitle: &str, show_nodes: bool) -> String {
    fn escape(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }
    let title = escape(title);
    let subtitle = escape(subtitle);
    let nodes_hidden = if show_nodes { "" } else { " hidden" };
    format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{title}</title><style>
:root{{--bg:#07130f;--panel:#0b1d17;--line:#214b3e;--text:#e6f4ef;--muted:#88a79d;--accent:#67d8b5}}*{{box-sizing:border-box}}body{{margin:0;background:var(--bg);color:var(--text);font:15px system-ui,sans-serif}}main{{max-width:1180px;margin:auto;padding:38px 24px 64px}}header{{display:flex;justify-content:space-between;gap:20px;align-items:end;margin-bottom:24px}}h1{{margin:4px 0 8px;font-size:34px}}p{{margin:0;color:var(--muted)}}button,select{{font:inherit}}.tabs{{display:inline-flex;gap:4px;padding:4px;border:1px solid var(--line);border-radius:10px;background:#091713}}.tab{{min-width:98px;border:0;border-radius:7px;padding:9px 15px;color:var(--muted);background:transparent;cursor:pointer}}.tab.active{{color:#07110f;background:var(--accent)}}.view[hidden]{{display:none}}.landing{{margin-top:20px;padding:28px}}.landing h2{{margin:0 0 9px}}label{{color:var(--muted)}}select{{margin-left:8px;background:var(--panel);color:var(--text);border:1px solid var(--line);padding:9px 13px;border-radius:8px}}.statistics-header{{display:flex;justify-content:space-between;gap:20px;align-items:center;margin-top:24px}}.statistics-header button{{border:1px solid var(--line);border-radius:8px;padding:9px 13px;color:var(--text);background:var(--panel);cursor:pointer}}.cards{{display:grid;grid-template-columns:repeat(auto-fit,minmax(210px,1fr));gap:12px;margin:20px 0}}article,.panel{{background:var(--panel);border:1px solid var(--line);border-radius:14px}}article{{padding:17px}}article small{{display:block;color:var(--muted);min-height:38px}}article strong{{display:block;margin-top:9px;font-size:24px;color:var(--accent)}}.panel{{margin-top:16px;overflow:hidden}}.panel h2{{font-size:18px;margin:0;padding:18px 20px;border-bottom:1px solid var(--line)}}.table{{overflow:auto}}table{{border-collapse:collapse;width:100%}}th,td{{text-align:left;padding:12px 16px;border-bottom:1px solid #17372d;white-space:nowrap}}th{{color:var(--muted);font-size:12px;text-transform:uppercase;letter-spacing:.06em}}td.dim{{white-space:normal;min-width:260px;color:var(--muted)}}.empty{{padding:24px;color:var(--muted)}}.error{{color:#ffaaa0}}@media(max-width:650px){{header,.statistics-header{{display:block}}label{{display:block;margin-top:16px}}}}
</style></head><body><main><header><div><p>FlowSplice · local node</p><h1>{title}</h1><p>{subtitle}</p></div></header><nav class="tabs" aria-label="Pages"><button type="button" class="tab active" data-page="overview">Overview</button><button type="button" class="tab" data-page="statistics">Statistics</button></nav><section id="overview-page" class="view"><article class="landing"><h2>Local node dashboard</h2><p>Statistics stay unloaded until you open the Statistics page.</p></article></section><section id="statistics-page" class="view" hidden><div class="statistics-header"><div id="status"></div><div><label>Report window<select id="period"><option value="day">Day</option><option value="week">Week</option><option value="month">Month</option><option value="year">Year</option></select></label><button id="refresh" type="button">Refresh</button></div></div><section id="overview" class="cards"></section><section class="panel"><h2>Business and Relay-path breakdown</h2><div class="table"><table><thead><tr><th>Metric</th><th>Dimensions</th><th>Total</th><th>Events</th><th>Weighted average</th><th>5-minute average</th></tr></thead><tbody id="breakdowns"></tbody></table></div></section><section class="panel"{nodes_hidden}><h2>Reporter freshness and completeness</h2><div class="table"><table><thead><tr><th>Node</th><th>Role</th><th>Reports</th><th>Families</th><th>Last bucket</th><th>Age</th><th>Missing intervals</th></tr></thead><tbody id="nodes"></tbody></table></div></section><section class="panel"><h2>Five-minute series</h2><div class="table"><table><thead><tr><th>Bucket (UTC)</th><th>Metric</th><th>Dimensions</th><th>Sum</th><th>Count</th><th>Average</th></tr></thead><tbody id="series"></tbody></table></div></section></section></main><script>
const e=v=>String(v??'').replace(/[&<>"']/g,c=>({{'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}}[c]));
const n=v=>new Intl.NumberFormat().format(Number(v||0));const f=v=>Number(v||0).toLocaleString(undefined,{{maximumFractionDigits:2}});const name=v=>String(v).replaceAll('_',' ');const dims=o=>Object.entries(o||{{}}).map(([k,v])=>`${{e(k)}}=${{e(v)}}`).join(' · ')||'all business';
async function render(){{const period=document.querySelector('#period').value;const status=document.querySelector('#status');status.textContent='Loading…';status.className='';try{{const response=await fetch('/api/statistics?period='+period,{{headers:{{Accept:'application/json'}}}});if(!response.ok)throw new Error('HTTP '+response.status);const data=await response.json();status.textContent=`UTC ${{new Date(data.from_unix_secs*1000).toISOString()}} – ${{new Date(data.to_unix_secs*1000).toISOString()}}${{data.dropped_events?` · dropped local events: ${{n(data.dropped_events)}}`:''}}`;document.querySelector('#overview').innerHTML=(data.overview||[]).map(x=>`<article><small>${{e(name(x.metric_family))}}</small><strong>${{n(x.sum)}}</strong><p>${{n(x.count)}} events · ${{f(x.average_per_five_minutes)}} / 5 min</p></article>`).join('')||'<p class="empty">No business observations in this window.</p>';document.querySelector('#breakdowns').innerHTML=(data.breakdowns||[]).map(x=>`<tr><td>${{e(name(x.metric_family))}}</td><td class="dim">${{dims(x.dimensions)}}</td><td>${{n(x.sum)}}</td><td>${{n(x.count)}}</td><td>${{f(x.weighted_average)}}</td><td>${{f(x.average_per_five_minutes)}}</td></tr>`).join('')||'<tr><td colspan="6" class="empty">No breakdown rows.</td></tr>';const points=(data.points||data.reports?.map(r=>({{bucket_start_unix_secs:r.payload.bucket_start_unix_secs,identity:{{metric_family:r.payload.metric_family,dimensions:r.payload.dimensions}},value:r.payload.value}}))||[]).sort((a,b)=>b.bucket_start_unix_secs-a.bucket_start_unix_secs).slice(0,200);document.querySelector('#series').innerHTML=points.map(x=>`<tr><td>${{e(new Date(x.bucket_start_unix_secs*1000).toISOString())}}</td><td>${{e(name(x.identity.metric_family))}}</td><td class="dim">${{dims(x.identity.dimensions)}}</td><td>${{n(x.value.sum)}}</td><td>${{n(x.value.count)}}</td><td>${{f(x.value.count?x.value.sum/x.value.count:0)}}</td></tr>`).join('')||'<tr><td colspan="6" class="empty">No five-minute points.</td></tr>';const nodes=document.querySelector('#nodes');if(nodes)nodes.innerHTML=(data.nodes||[]).map(x=>`<tr><td>${{e(x.reporter_id)}}</td><td>${{e(x.reporter_role)}}</td><td>${{n(x.report_count)}}</td><td>${{n(x.metric_family_count)}}</td><td>${{e(new Date(x.last_bucket_start_unix_secs*1000).toISOString())}}</td><td>${{n(x.last_report_age_secs)}}s</td><td>${{n(x.missing_five_minute_intervals)}}</td></tr>`).join('')||'<tr><td colspan="7" class="empty">No accepted node reports.</td></tr>'}}catch(error){{status.textContent='Unable to load statistics: '+error;status.className='error'}}}}
let refreshTimer;function activate(page){{const statistics=page==='statistics';document.querySelector('#overview-page').hidden=statistics;document.querySelector('#statistics-page').hidden=!statistics;document.querySelectorAll('.tab').forEach(button=>button.classList.toggle('active',button.dataset.page===page));if(statistics){{render();if(!refreshTimer)refreshTimer=setInterval(render,30000)}}else if(refreshTimer){{clearInterval(refreshTimer);refreshTimer=undefined}}}}document.querySelectorAll('.tab').forEach(button=>button.addEventListener('click',()=>activate(button.dataset.page)));document.querySelector('#period').addEventListener('change',render);document.querySelector('#refresh').addEventListener('click',render);activate('overview');
</script></body></html>"#
    )
}
pub const STATISTICS_METRIC_VERSION: u32 = 1;
pub const FIVE_MINUTE_SECS: u64 = 300;
pub const MAX_STATISTICS_DIMENSIONS: usize = 12;
pub const MAX_STATISTICS_DIMENSION_BYTES: usize = 96;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricValue {
    pub version: u32,
    pub revision: u64,
    pub count: u64,
    pub sum: u64,
    pub min: u64,
    pub max: u64,
    pub histogram: Vec<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StatisticsReportPayload {
    pub version: u32,
    pub deployment_id: String,
    pub reporter_role: Role,
    pub reporter_id: String,
    pub bucket_start_unix_secs: u64,
    pub bucket_end_unix_secs: u64,
    pub metric_family: String,
    pub dimensions: BTreeMap<String, String>,
    pub report_sequence: u64,
    pub value: MetricValue,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignedStatisticsReport {
    pub certificate_pem: String,
    pub payload_hex: String,
    pub signature_hex: String,
}

impl SignedStatisticsReport {
    /// Signs one validated five-minute statistics report.
    ///
    /// # Errors
    ///
    /// Returns an error when the payload or certificate is invalid, serialization fails, or the
    /// signing operation fails.
    pub fn sign(
        payload: &StatisticsReportPayload,
        certificate_pem: &str,
        key: &EcdsaKeyPair,
    ) -> Result<Self> {
        payload.validate()?;
        if certificate_pem.is_empty() || certificate_pem.len() > 64 * 1024 {
            bail!("statistics signing certificate is missing or oversized");
        }
        let bytes = serde_json::to_vec(payload).context("failed to encode statistics report")?;
        let signature = key
            .sign(&SystemRandom::new(), &bytes)
            .map_err(|_| anyhow!("failed to sign statistics report"))?;
        Ok(Self {
            certificate_pem: certificate_pem.to_owned(),
            payload_hex: hex::encode(bytes),
            signature_hex: hex::encode(signature.as_ref()),
        })
    }

    /// Verifies and decodes one statistics report with the supplied public key.
    ///
    /// # Errors
    ///
    /// Returns an error when hexadecimal decoding, signature verification, JSON decoding, or
    /// payload validation fails.
    pub fn verify(&self, public_key: &[u8]) -> Result<VerifiedStatisticsReport> {
        let payload_bytes = hex::decode(&self.payload_hex)
            .context("statistics report payload must be hexadecimal")?;
        let signature = hex::decode(&self.signature_hex)
            .context("statistics report signature must be hexadecimal")?;
        aws_lc_rs::signature::UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, public_key)
            .verify(&payload_bytes, &signature)
            .map_err(|_| anyhow!("statistics report has an invalid signature"))?;
        let payload: StatisticsReportPayload = serde_json::from_slice(&payload_bytes)
            .context("statistics report payload is invalid")?;
        payload.validate()?;
        Ok(VerifiedStatisticsReport {
            digest_sha256: sha256_hex(&payload_bytes),
            payload,
        })
    }

    /// Computes the SHA-256 digest of the encoded report payload.
    ///
    /// # Errors
    ///
    /// Returns an error when the payload is not valid hexadecimal.
    pub fn digest_sha256(&self) -> Result<String> {
        let payload = hex::decode(&self.payload_hex)
            .context("statistics report payload must be hexadecimal")?;
        Ok(sha256_hex(&payload))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedStatisticsReport {
    pub payload: StatisticsReportPayload,
    pub digest_sha256: String,
}

impl StatisticsReportPayload {
    /// Validates the bounded wire shape and aggregate invariants.
    ///
    /// # Errors
    ///
    /// Returns an error when a version, bucket, identity, dimension, sequence, or aggregate is
    /// outside the protocol contract.
    pub fn validate(&self) -> Result<()> {
        if self.version != STATISTICS_REPORT_VERSION
            || self.deployment_id.is_empty()
            || self.reporter_id.is_empty()
            || self.metric_family.is_empty()
            || self.metric_family.len() > MAX_STATISTICS_DIMENSION_BYTES
            || !self.bucket_start_unix_secs.is_multiple_of(FIVE_MINUTE_SECS)
            || self.bucket_end_unix_secs
                != self.bucket_start_unix_secs.saturating_add(FIVE_MINUTE_SECS)
            || self.report_sequence == 0
            || self.value.version != STATISTICS_METRIC_VERSION
            || self.value.revision == 0
            || self.dimensions.len() > MAX_STATISTICS_DIMENSIONS
        {
            bail!("statistics report shape is invalid");
        }
        for (key, value) in &self.dimensions {
            if key.is_empty()
                || value.is_empty()
                || key.len() > MAX_STATISTICS_DIMENSION_BYTES
                || value.len() > MAX_STATISTICS_DIMENSION_BYTES
            {
                bail!("statistics report dimension is invalid");
            }
        }
        if self.value.count == 0 {
            if self.value.sum != 0 || self.value.min != 0 || self.value.max != 0 {
                bail!("empty statistics report has non-zero aggregates");
            }
        } else if self.value.min > self.value.max {
            bail!("statistics report minimum exceeds maximum");
        }
        Ok(())
    }
}

/// Loads a P-256 statistics signer from the runtime management private key.
///
/// # Errors
///
/// Returns an error when the key is not a P-256 PKCS#8 private key.
pub fn statistics_signing_key(private_key: &PrivateKeyDer<'_>) -> Result<EcdsaKeyPair> {
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, private_key.secret_der())
        .map_err(|_| anyhow!("management key must be a P-256 PKCS#8 key for statistics signing"))
}

#[must_use]
pub const fn five_minute_bucket_start(unix_secs: u64) -> u64 {
    unix_secs - unix_secs % FIVE_MINUTE_SECS
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(digest::digest(&digest::SHA256, bytes).as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, KeyPair};

    #[test]
    fn dashboard_defers_statistics_until_second_page_is_opened() {
        let html = statistics_dashboard_html("Relay statistics", "Local Relay", false);
        assert!(html.contains("id=\"overview-page\" class=\"view\""));
        assert!(html.contains("id=\"statistics-page\" class=\"view\" hidden"));
        assert!(html.contains("data-page=\"statistics\""));
        assert!(html.contains("activate('overview')"));
        assert!(!html.contains("render();setInterval(render,30000)"));
    }

    #[test]
    fn signed_report_detects_tampering() -> Result<()> {
        let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)?;
        let payload = StatisticsReportPayload {
            version: STATISTICS_REPORT_VERSION,
            deployment_id: "deployment-1".to_owned(),
            reporter_role: Role::Relay,
            reporter_id: "relay-1".to_owned(),
            bucket_start_unix_secs: 300,
            bucket_end_unix_secs: 600,
            metric_family: "relay_transport_upload_bytes".to_owned(),
            dimensions: BTreeMap::from([("home_id".to_owned(), "home-1".to_owned())]),
            report_sequence: 1,
            value: MetricValue {
                version: STATISTICS_METRIC_VERSION,
                revision: 1,
                count: 1,
                sum: 42,
                min: 42,
                max: 42,
                histogram: Vec::new(),
            },
        };
        let report = SignedStatisticsReport::sign(&payload, "certificate", &key)?;
        assert_eq!(report.verify(key.public_key().as_ref())?.payload, payload);
        let mut tampered = report;
        tampered.payload_hex.push('0');
        assert!(tampered.verify(key.public_key().as_ref()).is_err());
        Ok(())
    }
}
