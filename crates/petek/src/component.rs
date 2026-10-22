//! Components and the registry that assigns them ids.

use std::alloc::Layout;
use std::any::{TypeId, type_name};

use crate::hash::FastHashMap;
use crate::storage::blob_vec::{DropFn, drop_fn_of};

/// Data that can be attached to an entity.
///
/// Any `Send + Sync + 'static` type can be a component:
///
/// ```
/// use petek::Component;
///
/// struct Position {
///     x: f32,
///     y: f32,
/// }
///
/// impl Component for Position {}
/// ```
///
/// Components must be `Send + Sync` so that worlds can be shared between
/// threads. This does not compile, because `Rc` is neither:
///
/// ```compile_fail,E0277
/// use std::rc::Rc;
///
/// struct Shared(Rc<u32>);
///
/// impl petek::Component for Shared {}
/// ```
pub trait Component: Send + Sync + 'static {}

/// Identifies a component type within one world.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ComponentId(u32);

/// Type-erased description of a component type.
///
/// The only way to create one is [`ComponentInfo::of`], so it always describes
/// an actual [`Component`]. Storage relies on this: columns are built from
/// these descriptions, and may be shared between threads because components
/// are `Send + Sync`.
#[derive(Clone, Copy, Debug)]
pub struct ComponentInfo {
    type_id: TypeId,
    name: &'static str,
    layout: Layout,
    drop: Option<DropFn>,
}

impl ComponentInfo {
    /// Describes the component type `C`.
    #[must_use]
    pub fn of<C: Component>() -> Self {
        Self {
            type_id: TypeId::of::<C>(),
            name: type_name::<C>(),
            layout: Layout::new::<C>(),
            drop: drop_fn_of::<C>(),
        }
    }

    /// The [`TypeId`] of the component type.
    #[must_use]
    pub fn type_id(&self) -> TypeId {
        self.type_id
    }

    /// The name of the component type, for diagnostics only.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The memory layout of the component type.
    #[must_use]
    pub fn layout(&self) -> Layout {
        self.layout
    }

    pub(crate) fn drop_fn(&self) -> Option<DropFn> {
        self.drop
    }
}

/// Assigns a [`ComponentId`] to each component type a world encounters.
///
/// This is only `pub` because the hidden methods of the query traits mention
/// it. It deliberately has no public constructor.
#[derive(Debug)]
pub struct Components {
    infos: Vec<ComponentInfo>,
    ids: FastHashMap<TypeId, ComponentId>,
}

impl Components {
    pub(crate) fn new() -> Self {
        Self {
            infos: Vec::new(),
            ids: FastHashMap::default(),
        }
    }

    /// Returns the id of the described component type, registering it on
    /// first use.
    pub(crate) fn register(&mut self, info: ComponentInfo) -> ComponentId {
        *self.ids.entry(info.type_id).or_insert_with(|| {
            let id = u32::try_from(self.infos.len()).expect("too many component types");
            self.infos.push(info);
            ComponentId(id)
        })
    }

    /// Returns the id of a component type, if it has been registered.
    #[inline]
    pub(crate) fn id(&self, type_id: TypeId) -> Option<ComponentId> {
        self.ids.get(&type_id).copied()
    }

    pub(crate) fn info(&self, id: ComponentId) -> &ComponentInfo {
        &self.infos[id.0 as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    struct Position(f32, f32);
    impl Component for Position {}

    #[allow(dead_code)]
    struct Name(String);
    impl Component for Name {}

    #[test]
    fn info_describes_the_type() {
        let info = ComponentInfo::of::<Position>();
        assert_eq!(info.type_id(), TypeId::of::<Position>());
        assert!(info.name().ends_with("Position"));
        assert_eq!(info.layout(), Layout::new::<Position>());
        assert!(info.drop_fn().is_none());
        assert!(ComponentInfo::of::<Name>().drop_fn().is_some());
    }

    #[test]
    fn each_type_is_registered_once() {
        let mut components = Components::new();
        let position = components.register(ComponentInfo::of::<Position>());
        let name = components.register(ComponentInfo::of::<Name>());

        assert_ne!(position, name);
        assert_eq!(
            components.register(ComponentInfo::of::<Position>()),
            position
        );
        assert_eq!(components.id(TypeId::of::<Name>()), Some(name));
        assert_eq!(components.id(TypeId::of::<u8>()), None);
        assert_eq!(components.info(name).type_id(), TypeId::of::<Name>());
    }
}
