//! Defines the algorithm for applying generated patches.

use std::collections::HashMap;

use rbx_dom_weak::{RbxId, RbxInstanceProperties, RbxValue};

use super::{
    patch::{AppliedPatchSet, AppliedPatchUpdate, PatchSet, PatchUpdate},
    InstancePropertiesWithMeta, InstanceSnapshot, RojoTree,
};

/// Consumes the input `PatchSet`, applying all of its prescribed changes to the
/// tree and returns an `AppliedPatchSet`, which can be used to keep another
/// tree in sync with Rojo's.
pub fn apply_patch_set(tree: &mut RojoTree, patch_set: PatchSet) -> AppliedPatchSet {
    let mut context = PatchApplyContext::default();

    for removed_id in patch_set.removed_instances {
        apply_remove_instance(&mut context, tree, removed_id);
    }

    for add_patch in patch_set.added_instances {
        apply_add_child(&mut context, tree, add_patch.parent_id, add_patch.instance);
    }

    // Updates need to be applied after additions, which reduces the complexity
    // of updates significantly.
    for update_patch in patch_set.updated_instances {
        apply_update_child(&mut context, tree, update_patch);
    }

    finalize_patch_application(context, tree)
}

/// All of the ephemeral state needing during application of a patch.
#[derive(Default)]
struct PatchApplyContext {
    /// A map from transient snapshot IDs (generated by snapshot middleware) to
    /// instance IDs in the actual tree. These are both the same data type so
    /// that they fit into the same `RbxValue::Ref` type.
    ///
    /// At this point in the patch process, IDs in instance properties have been
    /// partially translated from 'snapshot space' into 'tree space' by the
    /// patch computation process. An ID not existing in this map means either:
    ///
    /// 1. The ID is already in tree space and refers to an instance that
    ///    existed in the tree before this patch was applied.
    ///
    /// 2. The ID if in snapshot space, but points to an instance that was not
    ///    part of the snapshot that was put through the patch computation
    ///    function.
    ///
    /// #2 should not occur in well-formed projects, but is indistinguishable
    /// from #1 right now. It could happen if two model files try to reference
    /// eachother.
    snapshot_id_to_instance_id: HashMap<RbxId, RbxId>,

    /// The properties of instances added by the current `PatchSet`.
    ///
    /// Instances added to the tree can refer to eachother via Ref properties,
    /// but we need to make sure they're correctly transformed from snapshot
    /// space into tree space (via `snapshot_id_to_instance_id`).
    ///
    /// It's not possible to do that transformation for refs that refer to added
    /// instances until all the instances have actually been inserted into the
    /// tree. For simplicity, we defer application of _all_ properties on added
    /// instances instead of just Refs.
    ///
    /// This doesn't affect updated instances, since they're always applied
    /// after we've added all the instances from the patch.
    added_instance_properties: HashMap<RbxId, HashMap<String, RbxValue>>,

    /// The current applied patch result, describing changes made to the tree.
    applied_patch_set: AppliedPatchSet,
}

/// Finalize this patch application, consuming the context, applying any
/// deferred property updates, and returning the finally applied patch set.
///
/// Ref properties from snapshots refer to eachother via snapshot ID. Some of
/// these properties are transformed when the patch is computed, notably the
/// instances that the patch computing method is able to pair up.
///
/// The remaining Ref properties need to be handled during patch application,
/// where we build up a map of snapshot IDs to instance IDs as they're created,
/// then apply properties all at once at the end.
fn finalize_patch_application(context: PatchApplyContext, tree: &mut RojoTree) -> AppliedPatchSet {
    for (id, properties) in context.added_instance_properties {
        // This should always succeed since instances marked as added in our
        // patch should be added without fail.
        let mut instance = tree
            .get_instance_mut(id)
            .expect("Invalid instance ID in deferred property map");

        for (key, mut property_value) in properties {
            if let RbxValue::Ref { value: Some(id) } = property_value {
                if let Some(&instance_id) = context.snapshot_id_to_instance_id.get(&id) {
                    property_value = RbxValue::Ref {
                        value: Some(instance_id),
                    };
                }
            }

            instance.properties_mut().insert(key, property_value);
        }
    }

    context.applied_patch_set
}

fn apply_remove_instance(context: &mut PatchApplyContext, tree: &mut RojoTree, removed_id: RbxId) {
    match tree.remove_instance(removed_id) {
        Some(_) => context.applied_patch_set.removed.push(removed_id),
        None => {
            log::warn!(
                "Patch misapplication: Tried to remove instance {} but it did not exist.",
                removed_id
            );
        }
    }
}

fn apply_add_child(
    context: &mut PatchApplyContext,
    tree: &mut RojoTree,
    parent_id: RbxId,
    snapshot: InstanceSnapshot,
) {
    let properties = InstancePropertiesWithMeta {
        properties: RbxInstanceProperties {
            name: snapshot.name.into_owned(),
            class_name: snapshot.class_name.into_owned(),

            // Property assignment is deferred until after we know about all
            // instances in this patch. See `PatchApplyContext` for details.
            properties: HashMap::new(),
        },
        metadata: snapshot.metadata,
    };

    let id = tree.insert_instance(properties, parent_id);

    context.applied_patch_set.added.push(id);

    context
        .added_instance_properties
        .insert(id, snapshot.properties);

    if let Some(snapshot_id) = snapshot.snapshot_id {
        context.snapshot_id_to_instance_id.insert(snapshot_id, id);
    }

    for child_snapshot in snapshot.children {
        apply_add_child(context, tree, id, child_snapshot);
    }
}

fn apply_update_child(context: &mut PatchApplyContext, tree: &mut RojoTree, patch: PatchUpdate) {
    let mut applied_patch = AppliedPatchUpdate::new(patch.id);

    if let Some(metadata) = patch.changed_metadata {
        tree.update_metadata(patch.id, metadata.clone());
        applied_patch.changed_metadata = Some(metadata);
    }

    let mut instance = match tree.get_instance_mut(patch.id) {
        Some(instance) => instance,
        None => {
            log::warn!(
                "Patch misapplication: Instance {}, referred to by update patch, did not exist.",
                patch.id
            );
            return;
        }
    };

    if let Some(name) = patch.changed_name {
        *instance.name_mut() = name.clone();
        applied_patch.changed_name = Some(name);
    }

    if let Some(class_name) = patch.changed_class_name {
        *instance.class_name_mut() = class_name.clone();
        applied_patch.changed_class_name = Some(class_name);
    }

    for (key, property_entry) in patch.changed_properties {
        match property_entry {
            // Ref values need to be potentially rewritten from snapshot IDs to
            // instance IDs if they referred to an instance that was created as
            // part of this patch.
            Some(RbxValue::Ref { value: Some(id) }) => {
                // If our ID is not found in this map, then it either refers to
                // an existing instance NOT added by this patch, or there was an
                // error. See `PatchApplyContext::snapshot_id_to_instance_id`
                // for more info.
                let new_id = context
                    .snapshot_id_to_instance_id
                    .get(&id)
                    .copied()
                    .unwrap_or(id);

                instance.properties_mut().insert(
                    key.clone(),
                    RbxValue::Ref {
                        value: Some(new_id),
                    },
                );
            }
            Some(ref value) => {
                instance.properties_mut().insert(key.clone(), value.clone());
            }
            None => {
                instance.properties_mut().remove(&key);
            }
        }

        applied_patch.changed_properties.insert(key, property_entry);
    }

    context.applied_patch_set.updated.push(applied_patch)
}

#[cfg(test)]
mod test {
    use super::*;

    use std::{borrow::Cow, collections::HashMap};

    use maplit::hashmap;
    use rbx_dom_weak::RbxValue;

    use super::super::PatchAdd;

    #[test]
    fn add_from_empty() {
        let _ = env_logger::try_init();

        let mut tree = RojoTree::new(InstancePropertiesWithMeta {
            properties: RbxInstanceProperties {
                name: "Folder".to_owned(),
                class_name: "Folder".to_owned(),
                properties: HashMap::new(),
            },
            metadata: Default::default(),
        });

        let root_id = tree.get_root_id();

        let snapshot = InstanceSnapshot {
            snapshot_id: None,
            metadata: Default::default(),
            name: Cow::Borrowed("Foo"),
            class_name: Cow::Borrowed("Bar"),
            properties: hashmap! {
                "Baz".to_owned() => RbxValue::Int32 { value: 5 },
            },
            children: Vec::new(),
        };

        let patch_set = PatchSet {
            added_instances: vec![PatchAdd {
                parent_id: root_id,
                instance: snapshot.clone(),
            }],
            ..Default::default()
        };

        apply_patch_set(&mut tree, patch_set);

        let root_instance = tree.get_instance(root_id).unwrap();
        let child_id = root_instance.children()[0];
        let child_instance = tree.get_instance(child_id).unwrap();

        assert_eq!(child_instance.name(), &snapshot.name);
        assert_eq!(child_instance.class_name(), &snapshot.class_name);
        assert_eq!(child_instance.properties(), &snapshot.properties);
        assert!(child_instance.children().is_empty());
    }

    #[test]
    fn update_existing() {
        let _ = env_logger::try_init();

        let mut tree = RojoTree::new(InstancePropertiesWithMeta {
            properties: RbxInstanceProperties {
                name: "OldName".to_owned(),
                class_name: "OldClassName".to_owned(),
                properties: hashmap! {
                    "Foo".to_owned() => RbxValue::Int32 { value: 7 },
                    "Bar".to_owned() => RbxValue::Int32 { value: 3 },
                    "Unchanged".to_owned() => RbxValue::Int32 { value: -5 },
                },
            },
            metadata: Default::default(),
        });

        let root_id = tree.get_root_id();

        let patch = PatchUpdate {
            id: root_id,
            changed_name: Some("Foo".to_owned()),
            changed_class_name: Some("NewClassName".to_owned()),
            changed_properties: hashmap! {
                // The value of Foo has changed
                "Foo".to_owned() => Some(RbxValue::Int32 { value: 8 }),

                // Bar has been deleted
                "Bar".to_owned() => None,

                // Baz has been added
                "Baz".to_owned() => Some(RbxValue::Int32 { value: 10 }),
            },
            changed_metadata: None,
        };

        let patch_set = PatchSet {
            updated_instances: vec![patch],
            ..Default::default()
        };

        apply_patch_set(&mut tree, patch_set);

        let expected_properties = hashmap! {
            "Foo".to_owned() => RbxValue::Int32 { value: 8 },
            "Baz".to_owned() => RbxValue::Int32 { value: 10 },
            "Unchanged".to_owned() => RbxValue::Int32 { value: -5 },
        };

        let root_instance = tree.get_instance(root_id).unwrap();
        assert_eq!(root_instance.name(), "Foo");
        assert_eq!(root_instance.class_name(), "NewClassName");
        assert_eq!(root_instance.properties(), &expected_properties);
    }
}