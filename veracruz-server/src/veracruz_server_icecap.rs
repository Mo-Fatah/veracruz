//! IceCap-specific material for the Veracruz server
//!
//! ## Authors
//!
//! The Veracruz Development Team.
//!
//! ## Licensing and copyright notice
//!
//! See the `LICENSE_MIT.markdown` file in the Veracruz root directory for
//! information on licensing and copyright.

use crate::veracruz_server::{VeracruzServer, VeracruzServerError};
use bincode;
use err_derive::Error;
use io_utils::http::{post_buffer, send_proxy_attestation_server_start};
use policy_utils::policy::Policy;
use signal_hook::{
    consts::SIGINT,
    iterator::{Handle, Signals},
};
use std::{
    convert::TryFrom,
    env,
    error::Error,
    fs,
    io::{self, Read, Write},
    mem::size_of,
    os::unix::net::UnixStream,
    process::{Child, Command, Stdio},
    string::ToString,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::Duration,
};
use tempfile::{self, TempDir};
use veracruz_utils::runtime_manager_message::{
    RuntimeManagerRequest, RuntimeManagerResponse, Status,
};

const VERACRUZ_ICECAP_QEMU_BIN_DEFAULT: &'static [&'static str] = &["qemu-system-aarch64"];
const VERACRUZ_ICECAP_QEMU_FLAGS_DEFAULT: &'static [&'static str] = &[
    "-machine",
    "virt,virtualization=on,gic-version=2",
    "-cpu",
    "cortex-a57",
    "-smp",
    "4",
    "-m",
    "3072",
    "-semihosting-config",
    "enable=on,target=native",
    "-netdev",
    "user,id=netdev0",
    "-serial",
    "mon:stdio",
    "-nographic",
];
const VERACRUZ_ICECAP_QEMU_CONSOLE_FLAGS_DEFAULT: &'static [&'static str] = &[
    "-chardev",
    "socket,path={console0_path},server=on,wait=off,id=charconsole0",
    //"-chardev", "socket,server=on,host=localhost,port=1234,id=charconsole0",
    "-device",
    "virtio-serial-device",
    "-device",
    "virtconsole,chardev=charconsole0,id=console0",
];
const VERACRUZ_ICECAP_QEMU_IMAGE_FLAGS_DEFAULT: &'static [&'static str] =
    &["-kernel", "{image_path}"];

// Include image at compile time
const VERACRUZ_ICECAP_QEMU_IMAGE: &'static [u8] =
    include_bytes!(env!("VERACRUZ_ICECAP_QEMU_IMAGE"));

// TODO is this needed?
const FIRMWARE_VERSION: &str = "0.3.0";

/// Class of IceCap-specific errors.
#[derive(Debug, Error)]
pub enum IceCapError {
    #[error(display = "IceCap: Invalid environment variable value: {}", variable)]
    InvalidEnvironmentVariableValue { variable: String },
    #[error(display = "IceCap: Channel error: {}", _0)]
    ChannelError(io::Error),
    #[error(display = "IceCap: Qemu spawn error: {}", _0)]
    QemuSpawnError(io::Error),
    #[error(display = "IceCap: Serialization error: {}", _0)]
    SerializationError(bincode::Error),
    #[error(display = "IceCap: Unexpected response from runtime manager: {:?}", _0)]
    UnexpectedRuntimeManagerResponse(RuntimeManagerResponse),
}

impl From<IceCapError> for VeracruzServerError {
    fn from(err: IceCapError) -> VeracruzServerError {
        VeracruzServerError::IceCapError(err)
    }
}

impl From<bincode::Error> for VeracruzServerError {
    fn from(err: bincode::Error) -> VeracruzServerError {
        VeracruzServerError::from(IceCapError::SerializationError(err))
    }
}

/// IceCap implementation of 'VeracruzServer'
struct IceCapRealm {
    // NOTE the order of these fields matter due to drop ordering
    child: Arc<Mutex<Child>>,
    channel: UnixStream,
    #[allow(dead_code)]
    stdout_handler: JoinHandle<()>,
    #[allow(dead_code)]
    stderr_handler: JoinHandle<()>,
    signal_handle: Handle,
    #[allow(dead_code)]
    signal_handler: JoinHandle<()>,
    #[allow(dead_code)]
    tempdir: TempDir,
}

impl IceCapRealm {
    fn spawn() -> Result<IceCapRealm, VeracruzServerError> {
        fn env_flags(var: &str, default: &[&str]) -> Result<Vec<String>, VeracruzServerError> {
            match env::var(var) {
                Ok(var) => Ok(var.split_whitespace().map(|s| s.to_owned()).collect()),
                Err(env::VarError::NotPresent) => {
                    Ok(default.iter().map(|s| (*s).to_owned()).collect())
                }
                Err(_) => Err(IceCapError::InvalidEnvironmentVariableValue {
                    variable: var.to_owned(),
                })?,
            }
        }

        // Allow overriding these from environment variables
        let qemu_bin = env_flags("VERACRUZ_ICECAP_QEMU_BIN", VERACRUZ_ICECAP_QEMU_BIN_DEFAULT)?;
        let qemu_flags = env_flags(
            "VERACRUZ_ICECAP_QEMU_FLAGS",
            VERACRUZ_ICECAP_QEMU_FLAGS_DEFAULT,
        )?;
        let qemu_console_flags = env_flags(
            "VERACRUZ_ICECAP_QEMU_CONSOLE_FLAGS",
            VERACRUZ_ICECAP_QEMU_CONSOLE_FLAGS_DEFAULT,
        )?;
        let qemu_image_flags = env_flags(
            "VERACRUZ_ICECAP_QEMU_IMAGE_FLAGS",
            VERACRUZ_ICECAP_QEMU_IMAGE_FLAGS_DEFAULT,
        )?;

        // temporary directory for things
        let tempdir = tempfile::tempdir()?;

        // write the image to a temporary file, this makes sure our server is
        // idempotent
        let image_path = tempdir.path().join("image");
        fs::write(&image_path, VERACRUZ_ICECAP_QEMU_IMAGE)?;
        println!("vc-server: using image: {:?}", image_path);

        // create a temporary socket for communication
        let channel_path = tempdir.path().join("console0");
        println!("vc-server: using unix socket: {:?}", channel_path);

        // startup qemu
        let child = Arc::new(Mutex::new(
            Command::new(&qemu_bin[0])
                .args(&qemu_bin[1..])
                .args(&qemu_flags)
                .args(
                    qemu_console_flags
                        .iter()
                        .map(|s| s.replace("{console0_path}", channel_path.to_str().unwrap())),
                )
                .args(
                    qemu_image_flags
                        .iter()
                        .map(|s| s.replace("{image_path}", image_path.to_str().unwrap())),
                )
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(IceCapError::QemuSpawnError)?,
        ));

        // forward stderr/stdin via threads, this is necessary to avoid stdio
        // issues under Cargo test
        let stdout_handler = thread::spawn({
            let mut child_stdout = child.lock().unwrap().stdout.take().unwrap();
            move || {
                let err = io::copy(&mut child_stdout, &mut io::stdout());
                eprintln!("vc-server: qemu: stdout closed: {:?}", err);
            }
        });

        let stderr_handler = thread::spawn({
            let mut child_stderr = child.lock().unwrap().stderr.take().unwrap();
            move || {
                let err = io::copy(&mut child_stderr, &mut io::stderr());
                eprintln!("vc-server: qemu: stderr closed: {:?}", err);
            }
        });

        // hookup signal handler so SIGINT will teardown the child process
        let mut signals = Signals::new(&[SIGINT])?;
        let signal_handle = signals.handle();
        let signal_handler = thread::spawn({
            let child = child.clone();
            move || {
                for sig in signals.forever() {
                    eprintln!("vc-server: qemu: Killed by signal: {:?}", sig);
                    child.lock().unwrap().kill().unwrap();
                    signal_hook::low_level::emulate_default_handler(SIGINT).unwrap();
                }
            }
        });

        // connect via socket
        let channel = loop {
            match UnixStream::connect(&channel_path) {
                Ok(channel) => {
                    break channel;
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                // NOTE I don't know why this one happens
                Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => {
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Err(err) => {
                    Err(IceCapError::ChannelError(err))?;
                }
            };
        };

        Ok(IceCapRealm {
            child: child,
            stdout_handler: stdout_handler,
            stderr_handler: stderr_handler,
            signal_handle: signal_handle,
            signal_handler: signal_handler,
            channel: channel,
            tempdir: tempdir,
        })
    }

    fn communicate(
        &mut self,
        request: &RuntimeManagerRequest,
    ) -> Result<RuntimeManagerResponse, VeracruzServerError> {
        // send request
        let raw_request = bincode::serialize(request)?;
        let raw_header = bincode::serialize(&u32::try_from(raw_request.len()).unwrap())?;
        self.channel
            .write_all(&raw_header)
            .map_err(IceCapError::ChannelError)?;
        self.channel
            .write_all(&raw_request)
            .map_err(IceCapError::ChannelError)?;

        // recv response
        let mut raw_header = [0; size_of::<u32>()];
        self.channel
            .read_exact(&mut raw_header)
            .map_err(IceCapError::ChannelError)?;
        let header = bincode::deserialize::<u32>(&raw_header)?;
        let mut raw_response = vec![0; usize::try_from(header).unwrap()];
        self.channel
            .read_exact(&mut raw_response)
            .map_err(IceCapError::ChannelError)?;
        let response = bincode::deserialize::<RuntimeManagerResponse>(&raw_response)?;

        Ok(response)
    }

    // NOTE close can report errors, but drop can still happen in weird cases
    fn shutdown(self) -> Result<(), VeracruzServerError> {
        println!("vc-server: shutting down");
        self.signal_handle.close();
        self.child.lock().unwrap().kill()?;
        Ok(())
    }
}

pub struct VeracruzServerIceCap(Option<IceCapRealm>);

impl VeracruzServerIceCap {
    fn communicate(
        &mut self,
        request: &RuntimeManagerRequest,
    ) -> Result<RuntimeManagerResponse, VeracruzServerError> {
        match &mut self.0 {
            Some(realm) => realm.communicate(request),
            None => return Err(VeracruzServerError::UninitializedEnclaveError),
        }
    }

    fn tls_data_needed(&mut self, session_id: u32) -> Result<bool, VeracruzServerError> {
        match self.communicate(&RuntimeManagerRequest::GetTlsDataNeeded(session_id))? {
            RuntimeManagerResponse::TlsDataNeeded(needed) => Ok(needed),
            resp => Err(IceCapError::UnexpectedRuntimeManagerResponse(resp))?,
        }
    }
}

impl VeracruzServer for VeracruzServerIceCap {
    fn new(policy_json: &str) -> Result<Self, VeracruzServerError> {
        let policy: Policy = Policy::from_json(policy_json)?;

        // create the realm
        let mut self_ = Self(Some(IceCapRealm::spawn()?));

        let (device_id, challenge) = send_proxy_attestation_server_start(
            policy.proxy_attestation_server_url(),
            "psa",
            FIRMWARE_VERSION,
        )
        .map_err(VeracruzServerError::HttpError)?;

        let (token, csr) =
            match self_.communicate(&RuntimeManagerRequest::Attestation(challenge, device_id))? {
                RuntimeManagerResponse::AttestationData(token, csr) => (token, csr),
                resp => {
                    return Err(VeracruzServerError::IceCapError(
                        IceCapError::UnexpectedRuntimeManagerResponse(resp),
                    ))
                }
            };

        let (root_cert, compute_cert) = {
            let req =
                transport_protocol::serialize_native_psa_attestation_token(&token, &csr, device_id)
                    .map_err(VeracruzServerError::TransportProtocolError)?;
            let req = base64::encode(&req);
            let url = format!(
                "{:}/PSA/AttestationToken",
                policy.proxy_attestation_server_url()
            );
            let resp = post_buffer(&url, &req).map_err(VeracruzServerError::HttpError)?;
            let resp = base64::decode(&resp)?;
            let pasr = transport_protocol::parse_proxy_attestation_server_response(None, &resp)
                .map_err(VeracruzServerError::TransportProtocolError)?;
            let cert_chain = pasr.get_cert_chain();
            let root_cert = cert_chain.get_root_cert();
            let compute_cert = cert_chain.get_enclave_cert();
            (root_cert.to_vec(), compute_cert.to_vec())
        };

        match self_.communicate(&RuntimeManagerRequest::Initialize(
            policy_json.to_string(),
            vec![compute_cert, root_cert],
        ))? {
            RuntimeManagerResponse::Status(Status::Success) => (),
            resp => {
                return Err(VeracruzServerError::IceCapError(
                    IceCapError::UnexpectedRuntimeManagerResponse(resp),
                ))
            }
        }

        Ok(self_)
    }

    fn new_tls_session(&mut self) -> Result<u32, VeracruzServerError> {
        match self.communicate(&RuntimeManagerRequest::NewTlsSession)? {
            RuntimeManagerResponse::TlsSession(session_id) => Ok(session_id),
            resp => Err(VeracruzServerError::IceCapError(
                IceCapError::UnexpectedRuntimeManagerResponse(resp),
            )),
        }
    }

    fn close_tls_session(&mut self, session_id: u32) -> Result<(), VeracruzServerError> {
        match self.communicate(&RuntimeManagerRequest::CloseTlsSession(session_id))? {
            RuntimeManagerResponse::Status(Status::Success) => Ok(()),
            resp => Err(VeracruzServerError::IceCapError(
                IceCapError::UnexpectedRuntimeManagerResponse(resp),
            )),
        }
    }

    fn tls_data(
        &mut self,
        session_id: u32,
        input: Vec<u8>,
    ) -> Result<(bool, Option<Vec<Vec<u8>>>), VeracruzServerError> {
        match self.communicate(&RuntimeManagerRequest::SendTlsData(session_id, input))? {
            RuntimeManagerResponse::Status(Status::Success) => (),
            resp => {
                return Err(VeracruzServerError::IceCapError(
                    IceCapError::UnexpectedRuntimeManagerResponse(resp),
                ))
            }
        }

        let mut acc = Vec::new();
        let active = loop {
            if !self.tls_data_needed(session_id)? {
                break true;
            }
            match self.communicate(&RuntimeManagerRequest::GetTlsData(session_id))? {
                RuntimeManagerResponse::TlsData(data, active) => {
                    acc.push(data);
                    if !active {
                        break false;
                    }
                }
                resp => Err(IceCapError::UnexpectedRuntimeManagerResponse(resp))?,
            };
        };

        Ok((
            active,
            match acc.len() {
                0 => None,
                _ => Some(acc),
            },
        ))
    }

    fn shutdown_isolate(&mut self) -> Result<(), Box<dyn Error>> {
        match self.0.take() {
            Some(realm) => {
                realm.shutdown()?;
                Ok(())
            }
            None => Ok(()),
        }
    }
}

impl Drop for VeracruzServerIceCap {
    fn drop(&mut self) {
        if let Err(err) = self.shutdown_isolate() {
            panic!("Realm failed to shutdown: {}", err)
        }
    }
}
