use std::ffi::c_void;
use std::ops::DerefMut;
use std::ptr::{copy, write_bytes};
use bitfield_struct::bitfield;
use libc::{memcmp, memmove, size_t, write};

use crate::hyperion::components::container::{get_container_link_size, Container, ContainerLink, EmbeddedContainer, CONTAINER_MAX_FREESIZE};
use crate::hyperion::components::context::{ContainerTraversalContext, EmbeddedTraversalContext, JumpContext, OperationCommand, OperationContext, PathCompressedEjectionContext, RangeQueryContext};
use crate::hyperion::components::jump_table::{TopNodeJumpTable, SUBLEVEL_JUMPTABLE_SHIFTBITS};
use crate::hyperion::components::node::NodeType::{InnerNode, Invalid, LeafNodeEmpty, LeafNodeWithValue};
use crate::hyperion::components::node::{NodeType, NodeValue};
use crate::hyperion::components::return_codes::ReturnCode;
use crate::hyperion::components::return_codes::ReturnCode::{GetFailureNoLeaf, OK};
use crate::hyperion::components::sub_node::{ChildLinkType, SubNode};
use crate::hyperion::components::top_node::TopNode;
use crate::hyperion::internals::atomic_pointer::{AtomicChar, AtomicEmbContainer, AtomicHeader, AtomicNodeValue, AtomicPointer};
use crate::hyperion::internals::core::{initialize_ejected_container, HyperionCallback, GLOBAL_CONFIG};
use crate::hyperion::internals::helpers::{copy_memory_from, copy_memory_to};
use crate::memorymanager::api::{get_pointer, reallocate, HyperionPointer};

#[repr(C)]
#[derive(Clone, Copy)]
union NodeUnion {
    pub top_node: TopNode,
    pub sub_node: SubNode
}

pub struct NodeHeader {
    header: NodeUnion
}

impl NodeHeader {
    pub fn new_top_node(top_node: TopNode) -> Self {
        NodeHeader {
            header: NodeUnion { top_node }
        }
    }

    pub fn new_sub_node(sub_node: SubNode) -> Self {
        NodeHeader {
            header: NodeUnion { sub_node }
        }
    }

    pub fn as_raw(&self) -> *const NodeHeader {
        self as *const NodeHeader
    }

    pub fn as_raw_mut(&mut self) -> *mut NodeHeader {
        self as *mut NodeHeader
    }

    pub fn as_raw_char(&self) -> *const char {
        self.as_raw() as *const char
    }

    pub fn as_raw_char_mut(&self) -> *mut char {
        self.as_raw() as *mut char
    }

    pub fn as_raw_compressed(&self) -> *const PathCompressedNodeHeader {
        unsafe { self.as_raw().add(self.get_offset_child_container()) as *const PathCompressedNodeHeader }
    }

    pub fn as_raw_compressed_mut(&self) -> *mut PathCompressedNodeHeader {
        unsafe { self.as_raw().add(self.get_offset_child_container()) as *mut PathCompressedNodeHeader }
    }

    pub fn as_path_compressed(&self) -> &PathCompressedNodeHeader {
        unsafe { self.as_raw_compressed().as_ref().unwrap() }
    }

    pub fn as_path_compressed_mut(&self) -> &mut PathCompressedNodeHeader {
        unsafe { self.as_raw_compressed_mut().as_mut().unwrap() }
    }

    pub fn as_raw_embedded(&self, offset: usize) -> *const EmbeddedContainer {
        unsafe { self.as_raw().add(offset) as *const EmbeddedContainer }
    }

    pub fn as_top_node_mut(&mut self) -> &mut TopNode {
        unsafe { &mut self.header.top_node }
    }

    pub fn as_top_node(&self) -> &TopNode {
        unsafe { &self.header.top_node }
    }

    pub fn as_sub_node_mut(&mut self) -> &mut SubNode {
        unsafe { &mut self.header.sub_node }
    }

    pub fn as_sub_node(&self) -> &SubNode {
        unsafe { &self.header.sub_node }
    }

    pub fn get_jump_overhead(&self) -> u8 {
        self.as_top_node().jump_successor() * size_of::<u16>() as u8 + self.as_top_node().jump_table() * size_of::<TopNodeJumpTable>() as u8
    }

    pub fn get_leaf_size(&self) -> usize {
        match self.as_top_node().type_flag() {
            LeafNodeWithValue => size_of::<NodeValue>(),
            _ => 0
        }
    }

    pub fn get_offset_child_container(&self) -> usize {
        if self.as_top_node().delta() == 0 {
            return size_of::<NodeHeader>() + 1 + self.get_leaf_size();
        }
        size_of::<NodeHeader>() + self.get_leaf_size()
    }

    pub fn get_child_link_size(&self) -> usize {
        match self.as_sub_node().child_container() {
            ChildLinkType::None => 0,
            ChildLinkType::Link => size_of::<ContainerLink>(),
            ChildLinkType::EmbeddedContainer => unsafe { (*self.as_raw_embedded(self.get_offset_child_container())).size() as usize },
            ChildLinkType::PathCompressed => unsafe { (*self.as_raw_compressed()).size() as usize }
        }
    }

    pub fn get_offset_to_next_node(&self) -> usize {
        if self.as_top_node().is_top_node() {
            return self.get_offset_top_node();
        }
        self.get_offset_sub_node()
    }

    pub fn get_offset(&self) -> usize {
        if self.as_top_node().container_type() == 0 {
            return self.get_offset_top_node();
        }
        self.get_offset_sub_node()
    }

    pub fn get_offset_top_node(&self) -> usize {
        if !self.as_top_node().has_delta() {
            self.get_offset_top_node_nondelta()
        } else {
            self.get_offset_top_node_delta()
        }
    }

    pub fn get_offset_top_node_delta(&self) -> usize {
        size_of::<NodeHeader>() + self.get_jump_overhead() as usize + self.get_leaf_size()
    }

    pub fn get_offset_top_node_nondelta(&self) -> usize {
        self.get_offset_top_node_delta() + 1
    }

    pub fn get_offset_sub_node(&self) -> usize {
        if !self.as_top_node().has_delta() {
            self.get_offset_sub_node_nondelta()
        } else {
            self.get_offset_sub_node_delta()
        }
    }

    pub fn get_offset_sub_node_delta(&self) -> usize {
        size_of::<NodeHeader>() + self.get_jump_overhead() as usize + self.get_child_link_size()
    }

    pub fn get_offset_sub_node_nondelta(&self) -> usize {
        self.get_offset_sub_node_delta() + 1
    }

    pub fn get_offset_node_value(&self) -> usize {
        let base_size: usize = size_of::<NodeHeader>();
        if self.as_top_node().is_top_node() {
            return base_size + self.get_jump_overhead() as usize;
        }
        if !self.as_top_node().has_delta() {
            base_size + 1
        } else {
            base_size
        }
    }

    pub fn get_offset_jump(&self) -> usize {
        if !self.as_top_node().has_delta() {
            return size_of::<NodeHeader>() + 1;
        }
        size_of::<NodeHeader>()
    }

    pub fn get_jump_value(&self) -> u16 {
        let self_pointer: *const NodeHeader = self as *const NodeHeader;
        unsafe { *(self_pointer.add(self.get_offset_jump()) as *const u16) }
    }

    pub fn get_offset_jump_table(&self) -> u16 {
        self.get_offset_jump() as u16 + self.as_top_node().jump_successor() as u16 * size_of::<u16>() as u16
    }

    fn get_node_value_pc(&self, ocx: &mut OperationContext) -> ReturnCode {
        let pc_head: &PathCompressedNodeHeader = self.as_path_compressed();
        if pc_head.value_present() > 0 {
            unsafe {
                copy_memory_from(
                    pc_head.as_raw_char().add(size_of::<PathCompressedNodeHeader>()),
                    ocx.get_return_value_mut() as *mut NodeValue,
                    size_of::<NodeValue>()
                )
            }
        }
        ocx.header.set_operation_done(1);
        OK
    }

    pub fn get_node_value(&self, ocx: &mut OperationContext) -> ReturnCode {
        if ocx.header.pathcompressed_child() == 1 {
            return self.get_node_value_pc(ocx);
        }

        let top_node_type: NodeType = self.as_top_node().type_flag();

        if top_node_type == InnerNode || top_node_type == Invalid {
            return GetFailureNoLeaf;
        }

        if top_node_type == LeafNodeWithValue {
            unsafe {
                copy_memory_from(
                    self.as_raw_char().add(self.get_offset_node_value()),
                    ocx.get_return_value_mut() as *mut NodeValue,
                    size_of::<NodeValue>()
                );
            }
        }

        ocx.header.set_operation_done(1);
        OK
    }

    pub fn set_node_value(&mut self, ocx: &mut OperationContext) -> ReturnCode {
        let top_node: &mut TopNode = self.as_top_node_mut();

        if top_node.type_flag() == Invalid || top_node.type_flag() == InnerNode {
            ocx.header.set_performed_put(1);
        }

        if ocx.input_value.is_some() {
            let input_value: &mut NodeValue = ocx.get_input_value_mut();
            unsafe {
                copy_memory_to(self.as_raw_char_mut().add(self.get_offset_node_value()), input_value as *const NodeValue, size_of::<NodeValue>());
            }
            self.as_top_node_mut().set_type_flag(LeafNodeWithValue);
        } else {
            self.as_top_node_mut().set_type_flag(LeafNodeEmpty);
        }
        ocx.header.set_operation_done(1);
        OK
    }

    pub fn register_jump_context(&mut self, ctx: &mut ContainerTraversalContext, ocx: &mut OperationContext) {
        let jump_context: &mut JumpContext = ocx.get_jump_context_mut();
        if self.as_top_node().jump_successor() == 1 {
            jump_context.predecessor = Some(unsafe { Box::from_raw(self.as_raw_mut()) });//AtomicHeader::new_from_pointer(self.as_raw_mut());
            jump_context.sub_nodes_seen = 0;
            jump_context.top_node_predecessor_offset_absolute = ctx.current_container_offset;
        } else {
            jump_context.predecessor = None;
        }
    }

    pub fn call_top_node(&mut self, rqc: &mut RangeQueryContext, hyperion_callback: HyperionCallback<NodeValue>) -> bool {
        match self.as_top_node().type_flag() {
            LeafNodeEmpty => {
                hyperion_callback(&mut rqc.current_key, rqc.current_key_offset + 1, &mut AtomicNodeValue::new())
            },
            LeafNodeWithValue => unsafe {
                hyperion_callback(
                    &mut rqc.current_key,
                    rqc.current_key_offset + 1,
                    &mut AtomicNodeValue::new_from_pointer(self.as_raw_mut().add(self.get_offset_node_value()) as *mut NodeValue)
                )
            },
            Invalid | InnerNode => true
        }
    }

    pub fn call_sub_node(&mut self, range_query_context: &mut RangeQueryContext, hyperion_callback: HyperionCallback<NodeValue>) -> bool {
        match self.as_sub_node().type_flag() {
            LeafNodeEmpty => {
                hyperion_callback(&mut range_query_context.current_key, range_query_context.current_key_offset + 2, &mut AtomicNodeValue::new())
            },
            LeafNodeWithValue => unsafe {
                hyperion_callback(
                    &mut range_query_context.current_key,
                    range_query_context.current_key_offset + 2,
                    &mut AtomicNodeValue::new_from_pointer(self.as_raw_mut().add(self.get_offset_node_value()) as *mut NodeValue)
                )
            },
            Invalid | InnerNode => true
        }
    }

    pub fn compare_path_compressed_node(&self, ocx: &mut OperationContext) -> bool {
        let pc_header: &PathCompressedNodeHeader = unsafe { self.as_raw_compressed().as_ref().unwrap() };

        let overhead: usize = size_of::<PathCompressedNodeHeader>() + pc_header.value_present() as usize * size_of::<NodeValue>();
        let key_len: u8 = pc_header.size() - overhead as u8;

        if ocx.key_len_left - 2 != key_len as i32 {
            return false;
        }

        let op_key: &mut AtomicChar = ocx.get_key_as_mut();
        unsafe {
            let key: *const PathCompressedNodeHeader = (pc_header as *const PathCompressedNodeHeader).add(overhead);
            memcmp(op_key.add_get(2) as *mut c_void, key as *mut c_void, key_len as size_t) == 0
        }
    }

    pub fn use_sub_node_jump_table(&mut self, ctx: &mut ContainerTraversalContext) -> u8 {
        let jump_class = ctx.second_char >> SUBLEVEL_JUMPTABLE_SHIFTBITS;

        if jump_class > 0 {
            let jump_table_pointer: *mut u16 = unsafe { self.as_raw_mut().add(self.get_offset_jump_table() as usize) } as *mut u16;
            ctx.current_container_offset += self.get_offset() as i32 + unsafe { *jump_table_pointer + (jump_class as u16 - 1) } as i32;
            return jump_class << SUBLEVEL_JUMPTABLE_SHIFTBITS;
        }

        ctx.current_container_offset += self.get_offset() as i32;
        0
    }

    pub fn safe_path_compressed_context(&mut self, ocx: &mut OperationContext) {
        let pc_node = self.as_path_compressed();
        ocx.path_compressed_ejection_context = Some(PathCompressedEjectionContext::default());

        if pc_node.value_present() == 1 {
            unsafe {
                copy(
                    (pc_node as *const PathCompressedNodeHeader as *const c_void).add(size_of::<PathCompressedNodeHeader>()).add(size_of::<NodeValue>()) as *const u8,
                    ocx.path_compressed_ejection_context.as_mut().unwrap().partial_key.as_mut_ptr() as *mut u8,
                    pc_node.size() as usize - (size_of::<PathCompressedNodeHeader>() + size_of::<NodeValue>())
                );
                copy(
                    (pc_node as *const PathCompressedNodeHeader as *const c_void).add(size_of::<PathCompressedNodeHeader>()).add(size_of::<NodeValue>()) as *const u8,
                    &mut ocx.path_compressed_ejection_context.as_mut().unwrap().node_value as *mut NodeValue as *mut u8,
                    size_of::<NodeValue>()
                );
            }
        }
        else {
            unsafe {
                copy(
                    (pc_node as *const PathCompressedNodeHeader as *const c_void).add(size_of::<PathCompressedNodeHeader>()) as *const u8,
                    ocx.path_compressed_ejection_context.as_mut().unwrap().partial_key.as_mut_ptr() as *mut u8,
                    pc_node.size() as usize - size_of::<PathCompressedNodeHeader>()
                );
            }
        }
        ocx.path_compressed_ejection_context.as_mut().unwrap().pec_valid = 1;
        unsafe {
            copy(
                (pc_node as *const PathCompressedNodeHeader as *const c_void) as *const u8,
                &mut ocx.path_compressed_ejection_context.as_mut().unwrap().path_compressed_node_header as *mut PathCompressedNodeHeader as *mut u8,
                size_of::<PathCompressedNodeHeader>()
            );
        }
    }
}

pub fn update_path_compressed_node(mut node: Box<NodeHeader>, ocx: &mut OperationContext, ctx: &mut ContainerTraversalContext) -> Box<NodeHeader> {
    if let Some(_) = &mut ocx.input_value {
        let mut pc_node: &mut PathCompressedNodeHeader = node.as_path_compressed_mut();
        let mut value: *mut c_void = unsafe { (pc_node as *mut PathCompressedNodeHeader as *mut c_void).add(size_of::<PathCompressedNodeHeader>()) };

        if pc_node.value_present() == 0 {
            node = ocx.new_expand(ctx, size_of::<NodeValue>() as u32);
            let mut embedded_context: EmbeddedTraversalContext = ocx.embedded_traversal_context.take().unwrap();
            let mut root_container: &mut Container = embedded_context.root_container.as_mut();
            unsafe { root_container.wrap_shift_container(value, size_of::<NodeValue>()); }
            root_container.update_space_usage(size_of::<NodeValue>() as i16, ocx, ctx);
            ocx.embedded_traversal_context = Some(embedded_context);
            pc_node = node.as_path_compressed_mut();
            value = unsafe { (pc_node as *mut PathCompressedNodeHeader as *mut c_void).add(size_of::<PathCompressedNodeHeader>()) };
        }
        unsafe {  copy_memory_from(value, ocx.input_value.as_mut().unwrap().as_mut() as *mut NodeValue, size_of::<NodeValue>()); }
        pc_node.set_value_present(1);
    }
    node
}

pub fn eject_container(mut node: Box<NodeHeader>, ocx: &mut OperationContext, ctx: &mut ContainerTraversalContext) {
    assert!(ocx.embedded_traversal_context.as_mut().unwrap().embedded_container_depth > 0);
    node = ocx.meta_expand(ctx, get_container_link_size() as u32);
    let mut emb_context: EmbeddedTraversalContext = ocx.embedded_traversal_context.take().unwrap();
    let emb_container: &mut AtomicEmbContainer = &mut emb_context.embedded_stack[0];

    let child_offset: usize = node.get_offset_child_container();
    let embedded_container_offset = unsafe {
        (emb_context.root_container.as_mut() as *mut Container as *mut c_void).offset_from(emb_container.get_as_mut_memory()) as usize
    };
    let em_csize: u32 = unsafe { (*(emb_container.get())).size() as u32 };
    let ro_csize: u32 = emb_context.root_container.as_mut().size();
    assert!(ro_csize > em_csize);
    let ro_free_size_left = emb_context.root_container.as_mut().free_bytes();

    let mut container_ptr: HyperionPointer = initialize_ejected_container(ocx.arena.as_mut().unwrap().as_mut(), em_csize);
    let p_new: *mut Container = get_pointer(ocx.arena.as_mut().unwrap(), &mut container_ptr, 1, ctx.first_char) as *mut Container;
    unsafe {
        let target: *mut c_void = (p_new as *mut c_void).add((*p_new).get_container_head_size() as usize);
        let source: *mut c_void = emb_container.get_as_mut_memory().add(size_of::<EmbeddedContainer>());
        copy_memory_from(source, target, em_csize as usize - size_of::<EmbeddedContainer>());
        (*p_new).set_free_size_left((*p_new).free_bytes() as u32 - (em_csize - size_of::<EmbeddedContainer>() as u32));
    }
    node.as_sub_node_mut().set_child_container(ChildLinkType::Link);

    unsafe {
        let target2: *mut ContainerLink = (node.as_mut() as *mut NodeHeader as *mut c_void).add(child_offset) as *mut ContainerLink;
        (*target2).ptr = container_ptr;
    }

    let size: i32 = ro_csize as i32 - (em_csize as i32 + embedded_container_offset as i32 + ro_free_size_left as i32);
    emb_context.embedded_container_depth = 0;

    if size > 0 {
        unsafe {
            let node_ptr: *mut NodeHeader = node.as_mut() as *mut NodeHeader;
            let shift_dest: *mut c_void = (node_ptr as *mut c_void).add((*node_ptr).get_offset());
            let shift_src: *mut c_void = emb_container.get_as_mut_memory().add(em_csize as usize);
            memmove(shift_dest, shift_src, size as size_t);
        }
    }

    let delta: i32 = -1 * (em_csize as i32 - get_container_link_size() as i32);
    let new_free_size_left: i32 = emb_context.root_container.as_mut().free_bytes() as i32 - delta;
    emb_context.root_container.as_mut().update_space_usage(delta as i16, ocx, ctx);
    assert!(ro_csize as i32 > new_free_size_left);

    unsafe {
        let p_free = (emb_context.root_container.as_mut() as *mut Container as *mut c_void).add(ro_csize as usize - new_free_size_left as usize);
        write_bytes(p_free as *mut u8, 0, new_free_size_left as usize);
    }

    if new_free_size_left > CONTAINER_MAX_FREESIZE as i32 {
        let used = ro_csize as i32 - (ro_free_size_left as i32 - delta);
        assert!(used > 0);
        let container_increment = unsafe { GLOBAL_CONFIG.lock().unwrap().header.container_size_increment() as i32 };
        let mut tgt: u32 = (used / container_increment) as u32;
        if (used % container_increment) != 0 {
            tgt += 1;
        }
        let tgt_size: u32 = tgt * container_increment as u32;
        let new_free_size: u32 = (ro_free_size_left as u32 - delta as u32) % container_increment as u32;

        assert_eq!(emb_context.embedded_container_depth, 0);
        emb_context.root_container_pointer = reallocate(ocx.arena.as_mut().unwrap().as_mut(), &mut emb_context.root_container_pointer, tgt_size as usize, ocx.chained_pointer_hook);
        unsafe {
            emb_context.root_container = Box::from_raw(
                get_pointer(ocx.arena.as_mut().unwrap().as_mut(), &mut emb_context.root_container_pointer, 1, ocx.chained_pointer_hook) as *mut Container
            );
        }
        emb_context.root_container.set_free_size_left(new_free_size);
        emb_context.root_container.as_mut().set_size(tgt_size);
    }
    ocx.embedded_traversal_context = Some(emb_context);
}

pub fn add_embedded_container(mut node: Box<NodeHeader>, ocx: &mut OperationContext, ctx: &mut ContainerTraversalContext) {
    ocx.header.set_next_container_valid(2);
    let offset_child_container: usize = node.as_mut().get_offset_child_container();
    node = ocx.new_expand_embedded(ctx, size_of::<EmbeddedContainer>() as u32);
    let mut emb_context = ocx.embedded_traversal_context.take().unwrap();
    unsafe {
        emb_context.next_embedded_container = Box::from_raw(
            (node.as_mut() as *mut NodeHeader as *mut c_void).add(offset_child_container) as *mut EmbeddedContainer
        );

        emb_context.root_container.as_mut().wrap_shift_container(
            emb_context.next_embedded_container.as_mut() as *mut EmbeddedContainer as *mut c_void,
            size_of::<EmbeddedContainer>()
        );
    }
    ctx.current_container_offset += offset_child_container as i32;
    node.as_mut().as_sub_node_mut().set_child_container(ChildLinkType::EmbeddedContainer);
    ocx.embedded_traversal_context = Some(emb_context);
    ocx.safe_sub_node_jump_table_context(ctx);
    let mut emb_context = ocx.embedded_traversal_context.take().unwrap();
    emb_context.embedded_stack[emb_context.embedded_container_depth as usize] = AtomicEmbContainer::new_from_pointer(emb_context.next_embedded_container.as_mut() as *mut EmbeddedContainer);
    emb_context.embedded_container_depth += 1;
    ocx.next_container_pointer = None;
    emb_context.root_container.update_space_usage(size_of::<EmbeddedContainer>() as i16, ocx, ctx);
    ocx.embedded_traversal_context = Some(emb_context);
}

#[bitfield(u8, order = Msb)]
pub struct PathCompressedNodeHeader {
    #[bits(7)]
    pub size: u8,

    #[bits(1)]
    pub value_present: u8
}

impl PathCompressedNodeHeader {
    pub fn as_raw(&self) -> *const PathCompressedNodeHeader {
        self as *const PathCompressedNodeHeader
    }

    pub fn as_raw_char(&self) -> *const char {
        self.as_raw() as *const char
    }
}
