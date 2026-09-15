use crate::Boards;
use ducktape_module_sdk::WitStore;
ducktape_module_sdk::store_guest! {
    id: "boards",
    module: Boards,
    shape: ducktape_module_sdk::store_shape(),
    new: Boards::new(Box::new(WitStore)),
}
