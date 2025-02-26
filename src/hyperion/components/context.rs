use crate::hyperion::components::container::{get_container_link_size, shift_container, Container, EmbeddedContainer, RootContainerEntry, CONTAINER_MAX_EMBEDDED_DEPTH};
use crate::hyperion::components::node::NodeValue;
use crate::hyperion::components::node_header::{NodeHeader, PathCompressedNodeHeader};
use crate::hyperion::internals::atomic_pointer::{AtomicChar, AtomicContainer, AtomicEmbContainer, AtomicHyperionPointer, Atomicu8};
use crate::memorymanager::api::{get_pointer, reallocate, Arena, HyperionPointer};
use bitfield_struct::bitfield;
use std::ffi::c_void;
use std::ops::DerefMut;
use std::ptr::{null_mut, write_bytes};
use libc::setreuid;

#[derive(Debug)]
pub enum OperationCommand {
    Put = 0,
    Get = 1,
    Range = 2,
    Delete = 3,
}

impl OperationCommand {
    /// Transforms its states into a 2 bit representation.
    pub(crate) const fn into_bits(self) -> u8 {
        self as _
    }

    /// Transforms its states from an 8 bit value into a named state.
    ///
    /// # Panics
    /// Panics if an invalid operation type was found.
    pub(crate) const fn from_bits(value: u8) -> Self {
        match value {
            0 => OperationCommand::Put,
            1 => OperationCommand::Get,
            2 => OperationCommand::Range,
            3 => OperationCommand::Delete,
            _ => panic!("Use of undefined operation type"),
        }
    }
}

#[repr(packed)]
pub struct TraversalContext {
    pub offset: i32,
    pub hyperion_pointer: HyperionPointer,
}

#[bitfield(u8, order = Msb)]
pub struct ContainerTraversalHeader {
    #[bits(1)]
    pub node_type: u8,
    #[bits(1)]
    pub two_chars: u8,
    #[bits(1)]
    pub in_first_char_scope: u8,
    #[bits(1)]
    pub container_type: u8,
    #[bits(1)]
    pub last_top_char_set: u8,
    #[bits(1)]
    pub last_sub_char_set: u8,
    #[bits(1)]
    pub end_operation: u8,
    #[bits(1)]
    pub force_shift_before_insert: u8,
}

pub struct ContainerTraversalContext {
    pub header: ContainerTraversalHeader,
    pub last_top_char_seen: u8,
    pub last_sub_char_seen: u8,
    pub current_container_offset: i32,
    pub safe_offset: i32,
    pub first_char: u8,
    pub second_char: u8,
}

impl ContainerTraversalContext {
    pub fn flush(&mut self) {
        self.last_top_char_seen = 0;
        self.last_sub_char_seen = 0;
        self.current_container_offset = 0;
        self.header.set_in_first_char_scope(0);
    }
}

pub struct PathCompressedEjectionContext {
    pub node_value: NodeValue,
    pub partial_key: [char; 127],
    pub pec_valid: u8,
    pub path_compressed_node_header: PathCompressedNodeHeader,
}

impl Default for PathCompressedEjectionContext {
    fn default() -> Self {
        Self {
            node_value: NodeValue { v: 0 },
            partial_key: [char::from(0); 127],
            pec_valid: 0,
            path_compressed_node_header: PathCompressedNodeHeader::default(),
        }
    }
}

pub struct ContainerInjectionContext {
    pub root_container: AtomicContainer,
    pub container_pointer: AtomicHyperionPointer,
}

pub struct EmbeddedTraversalContext {
    pub root_container: Box<Container>,
    pub next_embedded_container: Box<EmbeddedContainer>,
    pub embedded_stack: [AtomicEmbContainer; CONTAINER_MAX_EMBEDDED_DEPTH],
    pub next_embedded_container_offset: i32,
    pub embedded_container_depth: i32,
    pub root_container_pointer: HyperionPointer,
}

pub struct JumpTableSubContext {
    pub top_node: Option<Box<NodeHeader>>,
    pub root_container_sub_char_set: u8,
    pub root_container_sub_char: char,
}

impl JumpTableSubContext {
    pub fn flush(&mut self) {
        self.top_node = None;
        self.root_container_sub_char = char::from(0);
        self.root_container_sub_char_set = 0;
    }
}

pub struct JumpContext {
    pub predecessor: Option<Box<NodeHeader>>,
    pub top_node_predecessor_offset_absolute: i32,
    pub sub_nodes_seen: i32,
    pub top_node_key: i32,
}

impl JumpContext {
    pub fn flush(&mut self) {
        self.predecessor = None;
        self.top_node_predecessor_offset_absolute = 0;
        self.sub_nodes_seen = 0;
        self.top_node_key = 0;
    }
}

pub struct RangeQueryContext {
    pub key_begin: AtomicChar,
    pub current_key: Atomicu8,
    pub arena: Box<Arena>,
    pub current_stack_depth: u16,
    pub current_key_offset: u16,
    pub key_len: u16,
    pub do_report: u8,
    pub stack: [Option<TraversalContext>; 128],
}

#[bitfield(u8, order = Msb)]
pub struct OperationContextHeader {
    #[bits(2)]
    pub command: OperationCommand,
    #[bits(2)]
    pub next_container_valid: u8,
    #[bits(1)]
    pub operation_done: u8,
    #[bits(1)]
    pub performed_put: u8,
    #[bits(1)]
    pub pathcompressed_child: u8,
    #[bits(1)]
    __: u8,
}

pub struct OperationContext {
    pub header: OperationContextHeader,
    pub chained_pointer_hook: u8,
    pub key_len_left: i32,
    pub key: Option<AtomicChar>,
    pub jump_context: Option<JumpContext>,
    pub root_container_entry: Option<Box<RootContainerEntry>>,
    pub embedded_traversal_context: Option<EmbeddedTraversalContext>,
    pub jump_table_sub_context: Option<JumpTableSubContext>,
    pub next_container_pointer: Option<Box<HyperionPointer>>,
    pub arena: Option<Box<Arena>>,
    pub path_compressed_ejection_context: Option<PathCompressedEjectionContext>,
    pub return_value: Option<Box<NodeValue>>,
    pub input_value: Option<Box<NodeValue>>,
    pub container_injection_context: Option<ContainerInjectionContext>,
}

impl OperationContext {
    pub fn flush_jump_context(&mut self) {
        if let Some(jump_context) = &mut self.jump_context {
            jump_context.flush();
        }
    }

    pub fn flush_jump_table_sub_context(&mut self) {
        if let Some(sub_context) = &mut self.jump_table_sub_context {
            sub_context.flush();
        }
    }

    pub fn get_return_value_mut(&mut self) -> &mut NodeValue {
        self.return_value.as_deref_mut().unwrap()
    }

    pub fn get_input_value_mut(&mut self) -> &mut NodeValue {
        self.input_value.as_deref_mut().unwrap()
    }

    pub fn get_jump_context_mut(&mut self) -> &mut JumpContext {
        self.jump_context.as_mut().unwrap()
    }

    pub fn get_key_as_mut(&mut self) -> &mut AtomicChar {
        self.key.as_mut().unwrap()
    }

    pub fn safe_sub_node_jump_table_context(&mut self, ctx: &mut ContainerTraversalContext) {
        let mut sub_jump_table: JumpTableSubContext = self.jump_table_sub_context.take().unwrap();

        if let Some(node) = sub_jump_table.top_node.as_deref_mut() {
            if node.as_top_node().jump_table() == 1 && self.embedded_traversal_context.as_mut().unwrap().embedded_container_depth == 0 {
                sub_jump_table.root_container_sub_char_set = 1;
                sub_jump_table.root_container_sub_char = char::from(ctx.second_char);
            }
        }
        self.jump_table_sub_context = Some(sub_jump_table);
    }

    pub fn new_expand(&mut self, ctx: &mut ContainerTraversalContext, required: u32) -> Box<NodeHeader> {
        let mut embedded_traversal_context: EmbeddedTraversalContext = self.embedded_traversal_context.take().unwrap();
        let mut arena: Box<Arena> = self.arena.take().unwrap();
        let free_space_left: u32 = embedded_traversal_context.root_container.deref_mut().free_bytes() as u32;

        if free_space_left > required {
            let mut old_size: u32;
            let mut new_size: u32;
            let mut sublevel_ref_toplevel_node_offset = 0;
            {
                let root_container: &mut Container = embedded_traversal_context.root_container.deref_mut();

                if let Some(jump_context) = &mut self.jump_table_sub_context {
                    if let Some(top_node) = jump_context.top_node.as_deref_mut() {
                        unsafe {
                            sublevel_ref_toplevel_node_offset =
                                (root_container as *mut Container as *mut c_void).offset_from(top_node as *mut NodeHeader as *mut c_void) as i32;
                        }
                    }
                }
                old_size = root_container.size();
                new_size = root_container.increment_container_size((required - free_space_left) as i32);
                assert_eq!(embedded_traversal_context.embedded_container_depth, 0);
                embedded_traversal_context.root_container_pointer =
                    reallocate(arena.as_mut(), &mut embedded_traversal_context.root_container_pointer, new_size as usize, self.chained_pointer_hook);
            }

            unsafe {
                embedded_traversal_context.root_container =
                    Box::from_raw(get_pointer(arena.as_mut(), &mut embedded_traversal_context.root_container_pointer, 1, self.chained_pointer_hook)
                        as *mut Container);
            }

            self.arena = Some(arena);

            embedded_traversal_context.root_container.set_free_size_left(new_size - old_size + free_space_left);

            let mut jump_context: JumpContext = self.jump_context.take().unwrap();
            let raw_container_pointer: *mut Container = embedded_traversal_context.root_container.as_mut() as *mut Container;

            if let Some(_) = jump_context.predecessor {
                unsafe {
                    jump_context.predecessor =
                        Some(Box::from_raw(raw_container_pointer.add(jump_context.top_node_predecessor_offset_absolute as usize) as *mut NodeHeader));
                }
            }
            self.jump_context = Some(jump_context);

            if sublevel_ref_toplevel_node_offset > 0 {
                unsafe {
                    self.jump_table_sub_context.as_mut().unwrap().top_node =
                        Some(Box::from_raw(raw_container_pointer.add(sublevel_ref_toplevel_node_offset as usize) as *mut NodeHeader));
                }
            }
        }

        let raw_container_pointer: *mut Container = embedded_traversal_context.root_container.as_mut() as *mut Container;
        self.embedded_traversal_context = Some(embedded_traversal_context);

        unsafe { Box::from_raw(raw_container_pointer.add(ctx.current_container_offset as usize) as *mut NodeHeader) }
    }

    pub fn new_expand_embedded(&mut self, ctx: &mut ContainerTraversalContext, required: u32) -> Box<NodeHeader> {
        let mut embedded_traversal_context: EmbeddedTraversalContext = self.embedded_traversal_context.take().unwrap();
        let mut arena: Box<Arena> = self.arena.take().unwrap();
        let free_space_left: u32 = embedded_traversal_context.root_container.deref_mut().free_bytes() as u32;

        if free_space_left > required {
            let mut i: usize = 0;
            let mut old_size: u32;
            let mut new_size: u32;
            let mut sublevel_ref_toplevel_node_offset = 0;
            let mut embedded_stack: [i32; CONTAINER_MAX_EMBEDDED_DEPTH] = [0; CONTAINER_MAX_EMBEDDED_DEPTH];
            {
                let root_container: &mut Container = embedded_traversal_context.root_container.deref_mut();

                if let Some(jump_context) = &mut self.jump_table_sub_context {
                    if let Some(top_node) = jump_context.top_node.as_deref_mut() {
                        unsafe {
                            sublevel_ref_toplevel_node_offset =
                                (root_container as *mut Container as *mut c_void).offset_from(top_node as *mut NodeHeader as *mut c_void) as i32;
                        }
                    }
                }

                unsafe {
                    for i in (i..embedded_traversal_context.next_embedded_container_offset as usize).rev() {
                        embedded_stack[i] = (root_container as *mut Container as *mut c_void).offset_from(embedded_traversal_context.embedded_stack[i].get_as_mut_memory()) as i32;
                    }
                }
                old_size = root_container.size();
                new_size = root_container.increment_container_size((required - free_space_left) as i32);
                root_container.set_free_size_left(0);
                embedded_traversal_context.root_container_pointer =
                    reallocate(arena.as_mut(), &mut embedded_traversal_context.root_container_pointer, new_size as usize, self.chained_pointer_hook);
            }

            unsafe {
                embedded_traversal_context.root_container =
                    Box::from_raw(get_pointer(arena.as_mut(), &mut embedded_traversal_context.root_container_pointer, 1, self.chained_pointer_hook)
                        as *mut Container);
            }

            self.arena = Some(arena);

            unsafe {
                let p_new: *mut c_void = (embedded_traversal_context.root_container.as_mut() as *mut Container as *mut c_void)
                    .add(old_size as usize);
                write_bytes(p_new as *mut u8, 0, (new_size - old_size) as usize);
                embedded_traversal_context.next_embedded_container = Box::from_raw(
                    (embedded_traversal_context.root_container.as_mut() as *mut Container as *mut c_void)
                        .add(embedded_traversal_context.next_embedded_container_offset as usize) as *mut EmbeddedContainer
                );

                let root_container: &mut Container = embedded_traversal_context.root_container.deref_mut();
                root_container.set_free_size_left((new_size - old_size) + free_space_left);

                for i in (i..embedded_traversal_context.embedded_container_depth as usize).rev() {
                    embedded_traversal_context.embedded_stack[i] =
                        AtomicEmbContainer::new_from_pointer(
                            (root_container as *mut Container as *mut c_void)
                                .add(embedded_stack[i] as usize) as *mut EmbeddedContainer);
                }

                if self.jump_context.as_mut().unwrap().predecessor.is_some() {
                    self.jump_context.as_mut().unwrap().predecessor = Some(
                        Box::from_raw(
                            (embedded_traversal_context.root_container.as_mut() as *mut Container as *mut c_void)
                                .add(self.jump_context.as_mut().unwrap().top_node_predecessor_offset_absolute as usize) as *mut NodeHeader
                        )
                    );
                }

                if sublevel_ref_toplevel_node_offset > 0 {
                    self.jump_table_sub_context.as_mut().unwrap().top_node =
                        Some(
                            Box::from_raw(
                                (embedded_traversal_context.root_container.as_mut() as *mut Container as *mut c_void)
                                    .add(sublevel_ref_toplevel_node_offset as usize) as *mut NodeHeader));
                }
            }

        }

        let mut raw_container_pointer: *mut Container = embedded_traversal_context.root_container.as_mut() as *mut Container;

        self.embedded_traversal_context = Some(embedded_traversal_context);
        unsafe {
            Box::from_raw(
                (raw_container_pointer as *mut char)
                    .add(ctx.current_container_offset as usize
                        + self.embedded_traversal_context.as_mut().unwrap().next_embedded_container_offset as usize) as *mut NodeHeader) }
    }

    pub fn insert_jump(&mut self, ctx: &mut ContainerTraversalContext, jump_value: u16) -> Box<NodeHeader> {
        self.new_expand(ctx, size_of::<NodeValue>() as u32);
        let node: *mut NodeHeader = unsafe {
            (self.embedded_traversal_context.as_mut().unwrap().root_container.as_mut() as *mut Container as *mut c_void )
                .add(self.jump_context.as_mut().unwrap().top_node_predecessor_offset_absolute as usize) as *mut NodeHeader
        };
        assert!(self.jump_context.as_mut().unwrap().top_node_predecessor_offset_absolute > 0);
        unsafe { assert_eq!((*node).as_top_node().container_type(), 0); }
        let free_size_left: usize = self.embedded_traversal_context.as_mut().unwrap().root_container.deref_mut().free_bytes() as usize;
        unsafe {
            let node_offset_to_jump: usize = (*node).get_offset_jump();
            let target: *mut c_void = (node as *mut c_void).add(node_offset_to_jump);
            shift_container(
                target,
                size_of::<u16>(),
                self.embedded_traversal_context.as_mut().unwrap().root_container.deref_mut().size() as usize -
                    (free_size_left + node_offset_to_jump + self.jump_context.as_mut().unwrap().top_node_predecessor_offset_absolute as usize));

            (*node).as_top_node_mut().set_jump_successor(1);
            *((node as *mut u16).add((*node).get_offset_jump())) += jump_value;
            let mut etc: EmbeddedTraversalContext = self.embedded_traversal_context.take().unwrap();
            let root_container: &mut Container = etc.root_container.as_mut();
            root_container.update_space_usage(size_of::<u16>() as i16, self, ctx);
            self.embedded_traversal_context = Some(etc);
            ctx.current_container_offset += size_of::<u16>() as i32;
            Box::from_raw((self.embedded_traversal_context.as_mut().unwrap().root_container.as_mut() as *mut Container as *mut c_void)
                .add(ctx.current_container_offset as usize) as *mut NodeHeader)
        }
    }

    pub fn meta_expand(&mut self, ctx: &mut ContainerTraversalContext, required: u32) -> Box<NodeHeader> {
        if self.embedded_traversal_context.as_mut().unwrap().embedded_container_depth == 0 {
            return self.new_expand(ctx, required);
        }
        self.new_expand_embedded(ctx, required)
    }
}
