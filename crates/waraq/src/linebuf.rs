pub const LINE: usize = 128;
#[derive(Debug)]
pub struct LineBuf<T> {
    raw: Vec<T>,
    start: usize,
    capacity: usize,
    len: usize,
}
impl<T: Copy + Default> Default for LineBuf<T> {
    fn default() -> Self {
        Self::new()
    }
}
impl<T: Copy + Default> LineBuf<T> {
    #[must_use]
    pub const fn new() -> Self {
        Self { raw: Vec::new(), start: 0, capacity: 0, len: 0 }
    }
    #[must_use]
    pub fn from_slice(from: &[T]) -> Self {
        let mut buf = Self::new();
        buf.extend_from_slice(from);
        buf
    }
    #[must_use]
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }
    #[must_use]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }
    #[inline]
    pub fn clear(&mut self) {
        self.len = 0;
    }
    #[must_use]
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        &self.raw[self.start..self.start + self.len]
    }
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.raw[self.start..self.start + self.len]
    }
    #[inline]
    pub fn resize(&mut self, n: usize) {
        self.reserve(n);
        let (from, to) = (self.start + self.len, self.start + n);
        if to > from {
            self.raw[from..to].fill(T::default());
        }
        self.len = n;
    }
    #[inline]
    pub fn extend_unfilled(&mut self, n: usize) {
        debug_assert!(n >= self.len, "extend_unfilled only grows the buffer");
        self.reserve(n);
        self.len = n;
    }
    #[inline]
    pub fn push(&mut self, value: T) {
        self.reserve(self.len + 1);
        self.raw[self.start + self.len] = value;
        self.len += 1;
    }
    #[inline]
    pub fn extend_from_slice(&mut self, from: &[T]) {
        let n = self.len + from.len();
        self.reserve(n);
        self.raw[self.start + self.len..self.start + n].copy_from_slice(from);
        self.len = n;
    }
    #[inline]
    fn reserve(&mut self, n: usize) {
        if n > self.capacity {
            self.grow(n);
        }
    }
    #[cold]
    #[inline(never)]
    fn grow(&mut self, n: usize) {
        let want = n.max(self.capacity.saturating_mul(2));
        let size = size_of::<T>().max(1);
        let mut raw = vec![T::default(); want + 2 * LINE.div_ceil(size) + 2];
        let base = raw.as_ptr() as usize;
        let first_line = base.next_multiple_of(LINE);
        let start = (first_line - base).div_ceil(size);
        let last_line = (base + raw.len() * size) / LINE * LINE;
        let capacity = (last_line - (base + start * size)) / size;
        debug_assert!(capacity >= want, "the slack covers the alignment at both ends");
        raw[start..start + self.len].copy_from_slice(self.as_slice());
        self.raw = raw;
        self.start = start;
        self.capacity = capacity;
    }
}
