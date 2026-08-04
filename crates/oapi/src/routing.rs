use std::any::TypeId;
use std::collections::BTreeSet;

use salvo_core::Router;
use salvo_core::http::Method;
use salvo_core::routing::FilterInfo;

use crate::SecurityRequirement;
use crate::path::PathItemType;

fn normalize_oapi_path(path: &str) -> String {
    let mut normalized = String::with_capacity(path.len());
    let mut chars = path.char_indices().peekable();

    while let Some((start, ch)) = chars.next() {
        if ch != '{' {
            normalized.push(ch);
            continue;
        }
        // Keep escaped literal braces (`{{`) as-is.
        if chars.peek().map(|(_, next)| *next) == Some('{') {
            normalized.push('{');
            normalized.push('{');
            chars.next();
            continue;
        }

        let content_start = start + ch.len_utf8();
        let mut braces_depth = 0usize;
        let mut escaping = false;
        let mut param_end = None;

        for (idx, current) in chars.by_ref() {
            if escaping {
                escaping = false;
                continue;
            }
            match current {
                '\\' => escaping = true,
                '{' => braces_depth += 1,
                '}' => {
                    if braces_depth == 0 {
                        param_end = Some(idx);
                        break;
                    }
                    braces_depth -= 1;
                }
                _ => {}
            }
        }

        if let Some(param_end) = param_end {
            let Some(content) = path.get(content_start..param_end) else {
                break;
            };
            let name_end = content.find([':', '|']).unwrap_or(content.len());
            let Some(name) = content.get(..name_end) else {
                break;
            };
            let name = ["*+", "*?", "**", "*"]
                .into_iter()
                .find_map(|prefix| name.strip_prefix(prefix))
                .unwrap_or(name);
            normalized.push('{');
            normalized.push_str(name);
            normalized.push('}');
        } else {
            if let Some(rest) = path.get(start..) {
                normalized.push_str(rest);
            }
            break;
        }
    }
    normalized
}

#[derive(Debug, Default)]
pub(crate) struct NormNode {
    pub(crate) handler_type_id: Option<TypeId>,
    pub(crate) handler_type_name: Option<&'static str>,
    pub(crate) method: Option<PathItemType>,
    pub(crate) path: Option<String>,
    pub(crate) children: Vec<Self>,
    pub(crate) metadata: Metadata,
}

#[derive(Debug, Default)]
struct RouteFilterMetadata {
    path: Option<String>,
    method: Option<PathItemType>,
}

impl RouteFilterMetadata {
    fn collect(router: &Router) -> Self {
        let mut metadata = Self::default();
        for filter in router.filters() {
            match filter.info() {
                FilterInfo::Path(path) => {
                    metadata.path = Some(normalize_oapi_path(&path));
                }
                FilterInfo::Method(method) => {
                    // Only overwrite when the method maps to a known
                    // `PathItemType`; unknown/extension methods leave any
                    // previously recognized value in place so combining a
                    // standard method filter with a custom one does not erase
                    // the standard mapping (the previous string-parsing path
                    // had the same behavior via its `_ => {}` arm).
                    //
                    // CONNECT is intentionally not mapped: OpenAPI 3.1 does
                    // not define a `connect` operation under Path Item, so
                    // emitting one would produce an invalid document.
                    let item = match method {
                        Method::GET => Some(PathItemType::Get),
                        Method::POST => Some(PathItemType::Post),
                        Method::PUT => Some(PathItemType::Put),
                        Method::DELETE => Some(PathItemType::Delete),
                        Method::HEAD => Some(PathItemType::Head),
                        Method::OPTIONS => Some(PathItemType::Options),
                        Method::TRACE => Some(PathItemType::Trace),
                        Method::PATCH => Some(PathItemType::Patch),
                        _ => None,
                    };
                    if item.is_some() {
                        metadata.method = item;
                    } else if method == Method::CONNECT {
                        tracing::warn!(
                            "HTTP CONNECT has no OpenAPI 3.1 mapping; the route will be \
                             omitted from the generated document"
                        );
                    }
                }
                // Other filter kinds (Scheme/Host/Port/Other) do not carry
                // information that maps to OpenAPI path items.
                _ => {}
            }
        }
        metadata
    }
}

impl NormNode {
    pub(crate) fn new(router: &Router, inherited_metadata: Metadata) -> Self {
        let route = RouteFilterMetadata::collect(router);
        let mut node = NormNode {
            path: route.path,
            method: route.method,
            metadata: inherited_metadata,
            ..NormNode::default()
        };
        if let Some(metadata) = router.extensions().get::<Metadata>() {
            node.metadata.tags.extend(metadata.tags.iter().cloned());
            node.metadata
                .securities
                .extend(metadata.securities.iter().cloned());
        }

        node.handler_type_id = router.goal.as_ref().map(|h| h.type_id());
        node.handler_type_name = router.goal.as_ref().map(|h| h.type_name());
        let routers = router.routers();
        if !routers.is_empty() {
            for router in routers {
                node.children.push(Self::new(router, node.metadata.clone()));
            }
        }
        node
    }
}

/// Router extension trait for openapi metadata.
pub trait RouterExt {
    /// Add security requirement to the router.
    ///
    /// All endpoints in the router and its descendants will inherit this security requirement.
    #[must_use]
    fn oapi_security(self, security: SecurityRequirement) -> Self;

    /// Add security requirements to the router.
    ///
    /// All endpoints in the router and its descendants will inherit these security requirements.
    #[must_use]
    fn oapi_securities<I>(self, security: I) -> Self
    where
        I: IntoIterator<Item = SecurityRequirement>;

    /// Add tag to the router.
    ///
    /// All endpoints in the router and its descendants will inherit this tag.
    #[must_use]
    fn oapi_tag(self, tag: impl Into<String>) -> Self;

    /// Add tags to the router.
    ///
    /// All endpoints in the router and its descendants will inherit these tags.
    #[must_use]
    fn oapi_tags<I, V>(self, tags: I) -> Self
    where
        I: IntoIterator<Item = V>,
        V: Into<String>;
}

impl RouterExt for Router {
    fn oapi_security(mut self, security: SecurityRequirement) -> Self {
        let metadata = self.extensions_mut().get_or_insert_default::<Metadata>();
        metadata.securities.push(security);
        self
    }
    fn oapi_securities<I>(mut self, iter: I) -> Self
    where
        I: IntoIterator<Item = SecurityRequirement>,
    {
        let metadata = self.extensions_mut().get_or_insert_default::<Metadata>();
        metadata.securities.extend(iter);
        self
    }
    fn oapi_tag(mut self, tag: impl Into<String>) -> Self {
        let metadata = self.extensions_mut().get_or_insert_default::<Metadata>();
        metadata.tags.insert(tag.into());
        self
    }
    fn oapi_tags<I, V>(mut self, iter: I) -> Self
    where
        I: IntoIterator<Item = V>,
        V: Into<String>,
    {
        let metadata = self.extensions_mut().get_or_insert_default::<Metadata>();
        metadata.tags.extend(iter.into_iter().map(Into::into));
        self
    }
}

#[non_exhaustive]
#[derive(Default, Clone, Debug)]
pub(crate) struct Metadata {
    pub(crate) tags: BTreeSet<String>,
    pub(crate) securities: Vec<SecurityRequirement>,
}

#[cfg(test)]
mod tests {
    use salvo_core::routing::{Filter, filters};
    use salvo_core::{Router, handler};

    use super::{Metadata, NormNode, RouterExt, normalize_oapi_path};
    use crate::{PathItemType, SecurityRequirement};

    #[handler]
    async fn test_handler() {}

    #[test]
    fn normalize_braced_path_constraints() {
        assert_eq!(normalize_oapi_path("/posts/{id}"), "/posts/{id}");
        assert_eq!(normalize_oapi_path("/posts/{id:num}"), "/posts/{id}");
        assert_eq!(
            normalize_oapi_path("/posts/{id:num(3..=10)}"),
            "/posts/{id}"
        );
        assert_eq!(normalize_oapi_path(r"/posts/{id|\d+}"), "/posts/{id}");
        assert_eq!(normalize_oapi_path("/posts/{id|[a-z]{2}}"), "/posts/{id}");
        assert_eq!(
            normalize_oapi_path("/posts/article_{id:num}"),
            "/posts/article_{id}"
        );
    }

    #[test]
    fn normalize_complex_path_parameters() {
        assert_eq!(
            normalize_oapi_path(r"/files/{name|[a-z]{2,4}}.{ext}"),
            "/files/{name}.{ext}"
        );
        assert_eq!(
            normalize_oapi_path(r"/posts/{id:num(3..=10)}/article_{slug|[a-z]{2}}"),
            "/posts/{id}/article_{slug}"
        );
        assert_eq!(normalize_oapi_path(r"/items/{id|foo\}bar}"), "/items/{id}");
        assert_eq!(normalize_oapi_path("/archive/{**rest}"), "/archive/{rest}");
        assert_eq!(normalize_oapi_path("/archive/{*+rest}"), "/archive/{rest}");
        assert_eq!(normalize_oapi_path("/archive/{*?rest}"), "/archive/{rest}");
    }

    #[test]
    fn norm_tree_collects_common_path_and_method_builders() {
        let router = Router::with_path("/users/{id:num}")
            .get(test_handler)
            .post(test_handler);

        let node = NormNode::new(&router, Metadata::default());

        assert_eq!(node.path.as_deref(), Some("/users/{id}"));
        assert_eq!(node.children.len(), 2);
        assert_eq!(node.children[0].method, Some(PathItemType::Get));
        assert_eq!(node.children[1].method, Some(PathItemType::Post));
        assert!(node.children.iter().all(|child| child.path.is_none()));
    }

    #[test]
    fn composite_filters_remain_opaque_without_core_metadata() {
        let router = Router::new()
            .filter(filters::path("/combined").and(filters::get()))
            .goal(test_handler);

        let node = NormNode::new(&router, Metadata::default());

        assert!(node.path.is_none());
        assert!(node.method.is_none());
        assert!(node.handler_type_id.is_some());
    }

    #[test]
    fn router_extensions_store_and_inherit_oapi_metadata() {
        let security = SecurityRequirement::new("oauth", ["read"]);
        let router = Router::new()
            .oapi_tag("root")
            .oapi_security(security.clone())
            .push(Router::new().oapi_tags(["child", "root"]))
            .push(Router::new());

        let attached = router
            .extensions()
            .get::<Metadata>()
            .expect("metadata should be stored on the router");
        assert_eq!(attached.tags.iter().collect::<Vec<_>>(), ["root"]);
        assert_eq!(
            attached.securities.as_slice(),
            std::slice::from_ref(&security)
        );

        let node = NormNode::new(&router, Metadata::default());
        assert_eq!(
            node.children[0].metadata.tags.iter().collect::<Vec<_>>(),
            ["child", "root"]
        );
        assert_eq!(
            node.children[0].metadata.securities.as_slice(),
            std::slice::from_ref(&security)
        );
        assert_eq!(
            node.children[1].metadata.tags.iter().collect::<Vec<_>>(),
            ["root"]
        );
        assert_eq!(
            node.children[1].metadata.securities.as_slice(),
            std::slice::from_ref(&security)
        );
    }
}
