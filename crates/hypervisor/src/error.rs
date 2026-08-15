use std::io;

/// libvirt's numeric `VIR_ERR_NO_DOMAIN` value.
pub const ERR_NO_DOMAIN: i32 = 42;
/// libvirt's numeric `VIR_ERR_NO_NETWORK` value.
pub const ERR_NO_NETWORK: i32 = 43;

/// An error returned in a libvirt RPC reply.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("libvirt error {code}: {message}")]
pub struct LibvirtError {
    /// The `virErrorNumber` carried by the remote error record.
    pub code: i32,
    /// The human-readable message supplied by libvirt.
    pub message: String,
}

/// An error from the hypervisor layer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("hypervisor: domain not found: {0}")]
    DomainNotFound(String),
    #[error("hypervisor: domain already exists: {0}")]
    DomainExists(String),
    #[error("{operation}: {source}")]
    Libvirt {
        operation: String,
        #[source]
        source: LibvirtError,
    },
    #[error("{operation}: {source}")]
    Io {
        operation: String,
        #[source]
        source: io::Error,
    },
    #[error("hypervisor protocol: {0}")]
    Protocol(String),
    #[error("{0}")]
    Operation(String),
}

impl Error {
    /// Reports whether this error wraps a libvirt error with `code`.
    pub fn is_libvirt_code(&self, code: i32) -> bool {
        matches!(self, Self::Libvirt { source, .. } if source.code == code)
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ApiError {
    #[error(transparent)]
    Libvirt(#[from] LibvirtError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("{0}")]
    Protocol(String),
}

pub(crate) fn operation_error(operation: impl Into<String>, source: ApiError) -> Error {
    let operation = operation.into();
    match source {
        ApiError::Libvirt(source) => Error::Libvirt { operation, source },
        ApiError::Io(source) => Error::Io { operation, source },
        ApiError::Protocol(message) => Error::Protocol(format!("{operation}: {message}")),
    }
}
