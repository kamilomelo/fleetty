#[derive(Clone, Copy)]
pub(crate) enum Aur {
    Paru,
    Yay,
    None,
}

impl Aur {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Paru => "paru",
            Self::Yay => "yay",
            Self::None => "none",
        }
    }

    pub(crate) fn command(self) -> &'static str {
        match self {
            Aur::Paru => "paru -Sua",
            Aur::Yay => "yay -Sua",
            Aur::None => "true",
        }
    }

    pub(crate) fn stage(self) -> &'static str {
        match self {
            Self::Paru => {
                "sudo -k || exit $?; echo '[fleetty] Starting AUR update: paru -Sua'; paru -Sua"
            }
            Self::Yay => {
                "sudo -k || exit $?; echo '[fleetty] Starting AUR update: yay -Sua'; yay -Sua"
            }
            Self::None => "echo '[fleetty] AUR NOT checked: no paru or yay detected'; true",
        }
    }
}
