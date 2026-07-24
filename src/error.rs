use std::{fmt::Display, process::ExitCode};

use abpl::{
	app::{axum::HotReloadAxumError, config::ParseTomlFileError},
	providers::ProvidesExitCode,
};

use crate::smtp_status::{ProvidesSmtpStatus, SmtpStatus, StatusClass, StatusSubject};

#[derive(Debug, Clone, abpl::Error)]
#[abpl_provider(ProvidesExitCode(1.into(), exit_code, ExitCode))]
pub enum ServiceErrorKind {
	#[cause(ParseTomlFileError)]
	#[abpl_provider(exit_code(cause))]
	Config,
	#[cause(HotReloadAxumError)]
	#[abpl_provider(exit_code(cause))]
	Http,
}
impl Display for ServiceErrorKind {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Config => f.write_str("config error"),
			Self::Http => f.write_str("failed to start http server"),
		}
	}
}

#[derive(Debug, Clone, abpl::Error)]
#[abpl_provider(ProvidesSmtpStatus(unreachable!(), smtp_status, SmtpStatus))]
pub enum ProxyTableLookupErrorKind {
	#[abpl_provider(smtp_status((StatusClass::PermFail, StatusSubject::SecurityOrPolicy, 50, 1).into()))]
	LookupFailed,
	#[abpl_provider(smtp_status((StatusClass::PermFail, StatusSubject::SecurityOrPolicy, 50, 1).into()))]
	AuthLookupFailed,
	#[cause(email_address::Error)]
	#[abpl_provider(smtp_status((StatusClass::PermFail, StatusSubject::Addressing, 1, 7).into()))]
	InvalidFrom,
	#[cause(email_address::Error)]
	#[abpl_provider(smtp_status((StatusClass::PermFail, StatusSubject::Addressing, 1, 3).into()))]
	InvalidRcpt,
}
impl Display for ProxyTableLookupErrorKind {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::LookupFailed => {
				f.write_str("neither the sending nor receiving domain is associated with this server")
			},
			Self::AuthLookupFailed => f.write_str("the sending domain is associated with this server"),
			Self::InvalidFrom => f.write_str("invalid sender"),
			Self::InvalidRcpt => f.write_str("invalid recipient"),
		}
	}
}
