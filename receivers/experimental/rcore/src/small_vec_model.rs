use std::cell::RefCell;

use slint::ModelNotify;
use smallvec::SmallVec;

pub struct SmallVecModel<T, const N: usize> {
    array: RefCell<SmallVec<[T; N]>>,
    notify: ModelNotify,
}

impl<T: Clone + 'static, const N: usize> slint::Model for SmallVecModel<T, N> {
    type Data = T;

    fn row_count(&self) -> usize {
        self.array.borrow().len()
    }

    fn row_data(&self, row: usize) -> Option<Self::Data> {
        self.array.borrow().get(row).cloned()
    }

    fn model_tracker(&self) -> &dyn slint::ModelTracker {
        &self.notify
    }

    fn set_row_data(&self, row: usize, data: Self::Data) {
        self.array.borrow_mut()[row] = data;
        self.notify.row_changed(row);
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
