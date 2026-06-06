use crate::KError;
use crate::mem::{Allocator, PoolAllocator};
use core::alloc::Layout;
use core::mem;
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;
use core::marker::PhantomData;
use core::fmt::{self, Debug};

pub type DynList<T> = List<T, PoolAllocator>;

pub struct ListIter<'a, T> {
    current: Option<&'a ListNode<T>>,
    head: Option<&'a ListNode<T>>
}

pub struct ListIterMut<'a, T> {
    current: Option<NonNull<ListNode<T>>>,
    head: Option<NonNull<ListNode<T>>>,
    _marker: PhantomData<&'a mut ListNode<T>>
}

pub struct ListNode<T> {
    data: T,
    prev: NonNull<ListNode<T>>,
    next: NonNull<ListNode<T>>
}

pub struct ListNodeGuard<T, A: Allocator<ListNode<T>>> {
    guard: NonNull<ListNode<T>>,
    _marker: PhantomData<A>
}

impl<T> ListNode<T> {
    pub fn into_inner<A: Allocator<ListNode<T>>>(guard_node: ListNodeGuard<T, A>) -> NonNull<ListNode<T>> {
        let guard_node = mem::ManuallyDrop::new(guard_node);
        guard_node.guard
    }
}

pub struct List<T, A: Allocator<ListNode<T>>> {
    head: Option<NonNull<ListNode<T>>>,
    tail: Option<NonNull<ListNode<T>>>,
    num_nodes: usize,
    _marker: PhantomData<A>
}

unsafe impl<T: Send, A: Allocator<ListNode<T>>> Send for List<T, A>{}

impl<T> Deref for ListNode<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl<T> DerefMut for ListNode<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}

impl<T, A: Allocator<ListNode<T>>> Default for List<T, A> {
    fn default() -> Self {
        List::new()
    }
}

impl<T, A: Allocator<ListNode<T>>> Deref for ListNodeGuard<T, A> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        unsafe { &self.guard.as_ref().data }
    }
}

impl<T, A: Allocator<ListNode<T>>> DerefMut for ListNodeGuard<T, A> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut self.guard.as_mut().data }
    }
}

impl<T, A: Allocator<ListNode<T>>> Drop for ListNodeGuard<T, A> {
    fn drop(&mut self) {
        unsafe {
            let size = mem::size_of::<ListNode<T>>();
            let align = mem::align_of::<ListNode<T>>();
            core::ptr::drop_in_place(self.guard.as_ptr());
            A::dealloc(self.guard, Layout::from_size_align(size, align).unwrap());
        }
    }
}

impl<T, A: Allocator<ListNode<T>>> Drop for List<T, A> {
    fn drop(&mut self) {
        self.clear();
    }
}

impl<T: Clone, A: Allocator<ListNode<T>>> Clone for List<T, A> {
    fn clone(&self) -> Self {
        let mut new_list = Self::new();

        for node in self.iter() {
            new_list.add_node(node.data.clone()).expect("List clone operation failed!");
        }

        new_list
    }
}

impl<T, A: Allocator<ListNode<T>>> List<T, A> {
    pub const fn new() -> Self {
        List {
            head: None,
            tail: None,
            num_nodes: 0,
            _marker: PhantomData
        }
    }

    pub fn first(&self) -> Option<&ListNode<T>> {
        if let Some(head) = self.head {
            unsafe { Some(&*head.as_ptr()) }
        }
        else {
            None
        }
    }

    pub fn first_mut(&mut self) -> Option<&mut ListNode<T>> {
        if let Some(head) = self.head {
            unsafe { Some(&mut *head.as_ptr()) }
        }
        else {
            None
        }
    }

    pub fn last(&self) -> Option<&ListNode<T>> {
        if let Some(tail) = self.tail {
            unsafe { Some(&*tail.as_ptr()) }
        }
        else {
            None
        }
    }


    pub fn add_node(&mut self, data: T) -> Result<(), KError> {
        self.add_node_get_handle(data).map(|_| ())
    }

    pub fn add_node_get_handle(&mut self, data: T) -> Result<NonNull<ListNode<T>>, KError> {
        let size  = mem::size_of::<ListNode<T>>();
        let align = mem::align_of::<ListNode<T>>();
        let ptr   = A::alloc(Layout::from_size_align(size, align).unwrap())?.as_ptr();
        let node  = NonNull::new(ptr).unwrap();

        unsafe {
            ptr.write(ListNode { data, prev: node, next: node });
        }

        self.insert_node_at_tail(node);
        Ok(node)
    }

    pub fn get_nodes(&self) -> usize {
        self.num_nodes
    }

    fn insert_node(&mut self, this: NonNull<ListNode<T>>, insert_at_tail: bool) {
        unsafe {
            let this_node = &mut *this.as_ptr();

            if self.num_nodes == 0 {
                this_node.next = this;
                this_node.prev = this;

                self.head = Some(this);
                self.tail = Some(this);
            }
            else {
                let head = self.head.unwrap();
                let tail = self.tail.unwrap();

                let head_node = &mut *head.as_ptr();
                let tail_node = &mut *tail.as_ptr();

                this_node.prev = tail;
                this_node.next = head;

                tail_node.next = this;
                head_node.prev = this;

                if insert_at_tail {
                    self.tail = Some(this);
                }
                else {
                    self.head = Some(this);
                }
            }

            self.num_nodes += 1;
        }
    }

    pub fn insert_node_at_tail(&mut self, this: NonNull<ListNode<T>>) {
        self.insert_node(this, true);
    }

    pub fn insert_node_at_head(&mut self, this: NonNull<ListNode<T>>) {
        self.insert_node(this, false);
    }

    pub fn pop_node(&mut self) {
        if let Some(node) = self.tail {
            unsafe {
                self.remove_node(node);
            }
        }
    }

    pub fn clear(&mut self) {
        while self.num_nodes != 0 {
            let node = self.head.unwrap();
            unsafe {
                self.remove_node(node);
            }
        }
    }

    // This is unsafe, since it is caller's responsibility to ensure that the given ListNode is a valid node that is
    // part of this list
    pub unsafe fn remove_node(&mut self, this: NonNull<ListNode<T>>) -> ListNodeGuard<T, A> {
        let node = unsafe { &mut *this.as_ptr() };

        if self.num_nodes == 1 {
            self.head = None;
            self.tail = None;
        }
        else {
            let prev = unsafe { &mut *node.prev.as_ptr() };
            let next = unsafe { &mut *node.next.as_ptr() };

            prev.next = node.next;
            next.prev = node.prev;

            if self.head.unwrap() == this {
                self.head = Some(node.next);
            }
            if self.tail.unwrap() == this {
                self.tail = Some(node.prev);
            }
        }

        self.num_nodes -= 1;
        ListNodeGuard { guard: this, _marker: PhantomData }
    }

    pub fn find_and_remove<F: Fn(&T) -> bool>(&mut self, predicate: F) -> Option<ListNodeGuard<T, A>> {
        let mut item = None;

        for node in self.iter() {
            if predicate(node) {
                item = Some(NonNull::from(node));
                break;
            }
        }

        if item.is_some() {
            unsafe {
                Some(self.remove_node(item.unwrap()))
            }
        }
        else {
            None
        }
    }

    pub fn iter(&self) -> ListIter<'_, T> {
        if let Some(head) = self.head {
            unsafe {
                ListIter {
                    current: Some(&*head.as_ptr()),
                    head: Some(&*head.as_ptr())
                }
            }
        }
        else {
            ListIter {
                current: None,
                head: None
            }
        }
    }

    pub fn iter_mut(&mut self) -> ListIterMut<'_, T> {
        ListIterMut {
            current: self.head,
            head: self.head,
            _marker: PhantomData
        }
    }
}

impl<'a, T> Iterator for ListIter<'a, T> {
    type Item = &'a ListNode<T>;
    fn next(&mut self) -> Option<Self::Item> {
        let cur = self.current?;
        let next = unsafe { &*cur.next.as_ptr() };

        // Pointer comparison — NOT PartialEq on T
        if core::ptr::eq(next, self.head.unwrap()) {
            self.current = None;
        }
        else {
            self.current = Some(next);
        }

        Some(cur)
    }
}

impl<'a, T> Iterator for ListIterMut<'a, T> {
    type Item = &'a mut ListNode<T>;
    fn next(&mut self) -> Option<Self::Item> {
        let cur = self.current?;
        let next = unsafe { (*cur.as_ptr()).next };

        if Some(next) == self.head {
            self.current = None;
        }
        else {
            self.current = Some(next);
        }

        unsafe {
            Some(&mut *cur.as_ptr())
        }
    }
}

impl<T: Debug, A: Allocator<ListNode<T>>> Debug for List<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut dbg = f.debug_struct("List");
        for desc in self.iter() {
            dbg.field("value", &desc.data);
        }
        dbg.finish()
    }
}
