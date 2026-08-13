use std::fmt::Display;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StatusClass {
	// Success = 2,
	TempFail = 4,
	// Intermediate omitted as that doesn't generalize to basic and extended code., Plus we don't actually ingest mail.
	PermFail = 5,
}
impl Display for StatusClass {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		(*self as u8).fmt(f)
	}
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StatusSubject {
	// Other = 0,
	Addressing = 1,
	// Mailbox = 2,
	MailSystem = 3,
	// NetworkAndRouting = 4,
	// MailDeliveryProtocol = 5,
	// MessageContentOrMedia = 6,
	SecurityOrPolicy = 7,
}
impl Display for StatusSubject {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		(*self as u8).fmt(f)
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmtpStatus {
	/// The `A` and `X` in `ABB X.Y.Z`
	pub class: StatusClass,

	/// The `Y` in `ABB X.Y.Z`
	pub subject: StatusSubject,

	/// The `BB` in `ABB X.Y.Z`
	pub basic_detail: u8,

	/// The `Z` in `ABB X.Y.Z`
	pub extended_detail: u8,
}
impl Display for SmtpStatus {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.write_fmt(format_args!(
			"{0}{2:02} {0}.{1}.{3}",
			self.class, self.subject, self.basic_detail, self.extended_detail
		))
	}
}
impl From<(StatusClass, StatusSubject, u8, u8)> for SmtpStatus {
	fn from(value: (StatusClass, StatusSubject, u8, u8)) -> Self {
		Self {
			class: value.0,
			subject: value.1,
			basic_detail: value.2,
			extended_detail: value.3,
		}
	}
}

pub trait ProvidesSmtpStatus {
	fn smtp_status(&self) -> SmtpStatus;
}
