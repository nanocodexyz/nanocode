use crate::error::Error;

pub const NAME_BYTES_MAX: usize = 253;
const FIELD_MANAGER_BYTES_MAX: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Resource {
    pub group: &'static str,
    pub version: &'static str,
    pub kind: &'static str,
    pub plural: &'static str,
    pub namespaced: bool,
}

pub const DEPLOYMENT: Resource = Resource {
    group: "apps",
    version: "v1",
    kind: "Deployment",
    plural: "deployments",
    namespaced: true,
};

pub const DAEMON_SET: Resource = Resource {
    group: "apps",
    version: "v1",
    kind: "DaemonSet",
    plural: "daemonsets",
    namespaced: true,
};

pub const SERVICE: Resource = Resource {
    group: "",
    version: "v1",
    kind: "Service",
    plural: "services",
    namespaced: true,
};

pub const CONFIG_MAP: Resource = Resource {
    group: "",
    version: "v1",
    kind: "ConfigMap",
    plural: "configmaps",
    namespaced: true,
};

pub const LEASE: Resource = Resource {
    group: "coordination.k8s.io",
    version: "v1",
    kind: "Lease",
    plural: "leases",
    namespaced: true,
};

pub const NODE: Resource = Resource {
    group: "",
    version: "v1",
    kind: "Node",
    plural: "nodes",
    namespaced: false,
};

impl Resource {
    pub fn api_version(&self) -> String {
        if self.group.is_empty() {
            self.version.to_owned()
        } else {
            format!("{}/{}", self.group, self.version)
        }
    }

    pub fn collection(&self, namespace: Option<&str>) -> Result<String, Error> {
        let root = if self.group.is_empty() {
            format!("/api/{}", self.version)
        } else {
            format!("/apis/{}/{}", self.group, self.version)
        };
        match (self.namespaced, namespace) {
            (true, Some(namespace)) => {
                check_name(namespace)?;
                Ok(format!("{root}/namespaces/{namespace}/{}", self.plural))
            }
            (false, None) => Ok(format!("{root}/{}", self.plural)),
            (true, None) => Err(Error::config(format!("{} needs a namespace", self.kind))),
            (false, Some(_)) => Err(Error::config(format!("{} is not namespaced", self.kind))),
        }
    }

    pub fn item(&self, namespace: Option<&str>, name: &str) -> Result<String, Error> {
        check_name(name)?;
        Ok(format!("{}/{name}", self.collection(namespace)?))
    }
}

pub fn check_name(name: &str) -> Result<(), Error> {
    let valid = !name.is_empty()
        && name.len() <= NAME_BYTES_MAX
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'.'
        })
        && !name.starts_with(['-', '.'])
        && !name.ends_with(['-', '.']);
    if valid {
        Ok(())
    } else {
        Err(Error::config(format!("invalid object name '{name}'")))
    }
}

pub fn check_field_manager(manager: &str) -> Result<(), Error> {
    let valid = !manager.is_empty()
        && manager.len() <= FIELD_MANAGER_BYTES_MAX
        && manager
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'));
    if valid {
        Ok(())
    } else {
        Err(Error::config(format!("invalid field manager '{manager}'")))
    }
}
