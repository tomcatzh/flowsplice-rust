//! Disposable-topology acceptance for directed business enrollment and socket startup.
use std::{path::Path, sync::Mutex, time::Duration};

use anyhow::{Context, Result, bail};
use flowsplice_core::{business::BusinessDescriptor, protocol::ServiceProtocol};
use flowsplice_enrollment::{business::BUSINESS_BINDING_FILE, load_json};
use flowsplice_storage::{StateStore, Table, WriteBatch};
use flowsplice_travel_core::{
    ServiceBinding, TravelCore,
    business::{BusinessEnrollmentOptions, enroll, load_binding},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() -> Result<()> {
    flowsplice_core::init_crypto();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("enroll") if args.len() == 8 => enroll_probe(&args[2..]).await,
        Some("travel") if args.len() == 6 => {
            tokio::time::timeout(Duration::from_secs(120), travel(&args[2..]))
                .await
                .context("business travel timed out")?
        }
        Some("check-only") if args.len() == 5 => {
            let descriptor = load_json(Path::new(&args[4]))?;
            let root = std::fs::read_to_string(&args[3])?;
            let binding = load_binding(Path::new(&args[2]), root.trim(), &descriptor)?;
            println!(
                "{}",
                serde_json::json!({"checkpoint":"business-binding-verified","request_id":binding.request_id,"credential_id":binding.credential_id})
            );
            Ok(())
        }
        Some("resume-fixture") if args.len() == 4 => {
            resume_fixture(Path::new(&args[2]), Path::new(&args[3]))
        }
        _ => bail!(
            "usage: business-probe enroll RELAY DESCRIPTOR ROOT INSTALL PASSWORD TRAVEL_ID | travel CONFIG PASSWORD ROOT DESCRIPTOR | check-only CONFIG ROOT DESCRIPTOR | resume-fixture INSTALL BOOTSTRAP_BACKUP"
        ),
    }
}

fn no_local_sockets() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let sockets: std::collections::HashSet<_> = std::fs::read_dir("/proc/self/fd")?
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .filter_map(|path| {
                Some(
                    path.to_str()?
                        .strip_prefix("socket:[")?
                        .strip_suffix(']')?
                        .to_owned(),
                )
            })
            .collect();
        for (path, udp) in [
            ("/proc/net/tcp", false),
            ("/proc/net/tcp6", false),
            ("/proc/net/udp", true),
            ("/proc/net/udp6", true),
        ] {
            for line in std::fs::read_to_string(path)?.lines().skip(1) {
                let fields: Vec<_> = line.split_whitespace().collect();
                if (udp || fields.get(3) == Some(&"0A"))
                    && fields.get(9).is_some_and(|inode| sockets.contains(*inode))
                {
                    bail!("business Travel owns a TCP listener or UDP socket");
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    bail!("business socket ownership acceptance requires Linux /proc");
    #[cfg(target_os = "linux")]
    Ok(())
}

async fn enroll_probe(args: &[String]) -> Result<()> {
    let descriptor: BusinessDescriptor = load_json(Path::new(&args[1]))?;
    let root = std::fs::read_to_string(&args[2])?;
    let password = std::fs::read_to_string(&args[4])?;
    let failure = Mutex::new(None);
    enroll(BusinessEnrollmentOptions {
        travel_id: args[5].clone(), install_dir: (&args[3]).into(), relay_address: args[0].clone(),
        deployment_root_public_key: root.trim().to_owned(), private_key_password: password.trim_end().to_owned(), wait_timeout_secs:120,
    }, descriptor, |progress| {
        if let Err(error) = no_local_sockets()
            && let Ok(mut failure) = failure.lock() { *failure = Some(error); }
        println!("{}", serde_json::json!({"phase":progress.phase,"travel_id":progress.travel_id,"request_id":progress.request_id,"credential_id":progress.credential_id,"verification_code":progress.verification_code}));
    }).await?;
    if let Some(error) = failure
        .into_inner()
        .map_err(|_| anyhow::anyhow!("progress check poisoned"))?
    {
        return Err(error);
    }
    no_local_sockets()?;
    println!("{{\"checkpoint\":\"business-enrollment-installed\"}}");
    Ok(())
}

fn state_store(config: &Path) -> Result<StateStore> {
    let config: toml::Value = toml::from_str(&std::fs::read_to_string(config)?)?;
    StateStore::open(
        config
            .get("state_store")
            .and_then(toml::Value::as_str)
            .context("missing state_store")?,
    )
}

async fn travel(args: &[String]) -> Result<()> {
    let config = Path::new(&args[0]);
    {
        let store = state_store(config)?;
        if store
            .get_json::<Vec<flowsplice_travel_core::Mapping>>(Table::TravelMappings, b"active")?
            .is_some_and(|mappings| !mappings.is_empty())
        {
            bail!("business installation contains legacy mappings");
        }
    }
    let password = std::fs::read_to_string(&args[1])?;
    let root = std::fs::read_to_string(&args[2])?;
    let descriptor = load_json(Path::new(&args[3]))?;
    let (runtime, approved) =
        TravelCore::start_business(config, password.trim_end(), root.trim(), &descriptor).await?;
    if approved.binding.protocol != ServiceProtocol::Tcp {
        bail!("fixture requires TCP business");
    }
    no_local_sockets()?;
    roundtrip(&runtime, &approved.binding, 0).await?;
    let (a, b, c, d) = tokio::join!(
        roundtrip(&runtime, &approved.binding, 1),
        roundtrip(&runtime, &approved.binding, 2),
        roundtrip(&runtime, &approved.binding, 3),
        roundtrip(&runtime, &approved.binding, 4)
    );
    a?;
    b?;
    c?;
    d?;
    for binding in [
        ServiceBinding {
            service_id: "business-probe-unknown".to_owned(),
            ..approved.binding.clone()
        },
        ServiceBinding {
            home_id: "business-probe-other-home".to_owned(),
            ..approved.binding.clone()
        },
    ] {
        if tokio::time::timeout(Duration::from_secs(15), runtime.connect_tcp(&binding))
            .await
            .context("denial timed out")?
            .is_ok()
        {
            bail!("unapproved binding connected");
        }
    }
    roundtrip(&runtime, &approved.binding, 5).await?;
    no_local_sockets()?;
    tokio::time::timeout(Duration::from_secs(5), runtime.shutdown())
        .await
        .context("business shutdown timed out")?;
    if runtime.status().await.active_flows != 0 {
        bail!("shutdown retained active flows");
    }
    println!("{{\"checkpoint\":\"business-in-process-sockets\"}}");
    Ok(())
}

async fn roundtrip(runtime: &TravelCore, binding: &ServiceBinding, tag: u8) -> Result<()> {
    let stream = runtime.connect_tcp(binding).await?;
    no_local_sockets()?;
    let payload: Vec<u8> = (0..131_071)
        .map(|index| if index % 2 == 0 { tag } else { 255 })
        .chain(*b"\n")
        .collect();
    let (reader, mut writer) = tokio::io::split(stream);
    let send = async {
        writer.write_all(&payload).await?;
        writer.shutdown().await
    };
    let receive = async {
        let mut output = Vec::new();
        reader.take(262_145).read_to_end(&mut output).await?;
        if output.len() > 262_144 || !output.ends_with(&payload) {
            bail!("binary echo or half-close mismatch");
        }
        Ok::<_, anyhow::Error>(())
    };
    let (write_result, read_result) = tokio::join!(send, receive);
    write_result?;
    read_result
}

fn resume_fixture(directory: &Path, backup: &Path) -> Result<()> {
    let marker = directory.join(BUSINESS_BINDING_FILE);
    let completed: serde_json::Value = load_json(&marker)?;
    let response = completed
        .get("response")
        .context("missing completed response")?;
    let request_id: uuid::Uuid = serde_json::from_value(
        response
            .pointer("/request/request/request_id")
            .context("missing request id")?
            .clone(),
    )?;
    let config = directory.join("travelagent.toml");
    let config_toml = std::fs::read_to_string(&config)?;
    let store = state_store(&config)?;
    std::fs::write(
        directory.join("business-installation.pending.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"response":response,"config_toml":config_toml}),
        )?,
    )?;
    std::fs::copy(backup, directory.join("bootstrap-enrollment.json"))?;
    std::fs::remove_file(marker)?;
    store.apply_immediate(
        WriteBatch::new().delete(Table::EnrollmentOutbox, request_id.as_bytes().to_vec()),
    )?;
    println!("{{\"checkpoint\":\"business-resume-fixture-prepared\"}}");
    Ok(())
}
