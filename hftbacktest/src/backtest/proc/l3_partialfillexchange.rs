use crate::{
    backtest::{
        BacktestError,
        assettype::AssetType,
        models::{FeeModel, L3QueueModel, LatencyModel},
        order::ExchToLocal,
        proc::Processor,
        state::State,
    },
    depth::L3MarketDepth,
    prelude::OrdType,
    types::{
        BUY_EVENT, EXCH_ASK_ADD_ORDER_EVENT, EXCH_ASK_DEPTH_CLEAR_EVENT, EXCH_BID_ADD_ORDER_EVENT,
        EXCH_BID_DEPTH_CLEAR_EVENT, EXCH_CANCEL_ORDER_EVENT, EXCH_DEPTH_CLEAR_EVENT, EXCH_EVENT,
        EXCH_FILL_EVENT, EXCH_MODIFY_ORDER_EVENT, Event, Order, OrderId, SELL_EVENT, Side, Status,
        TimeInForce,
    },
};

pub struct L3PartialFillExchange<AT, LM, QM, MD, FM>
where
    AT: AssetType,
    LM: LatencyModel,
    QM: L3QueueModel<MD>,
    MD: L3MarketDepth,
    FM: FeeModel,
{
    depth: MD,
    state: State<AT, FM>,
    queue_model: QM,
    order_e2l: ExchToLocal<LM>,
}

impl<AT, LM, QM, MD, FM> L3PartialFillExchange<AT, LM, QM, MD, FM>
where
    AT: AssetType,
    LM: LatencyModel,
    QM: L3QueueModel<MD>,
    MD: L3MarketDepth,
    FM: FeeModel,
    BacktestError: From<<MD as L3MarketDepth>::Error>,
{
    /// Constructs an instance of `L3PartialFillExchange`.
    pub fn new(
        depth: MD,
        state: State<AT, FM>,
        queue_model: QM,
        order_e2l: ExchToLocal<LM>,
    ) -> Self {
        Self {
            depth,
            state,
            queue_model,
            order_e2l,
        }
    }

    fn expired(&mut self, mut order: Order, timestamp: i64) -> Result<(), BacktestError> {
        order.exec_qty = 0.0;
        order.leaves_qty = 0.0;
        order.status = Status::Expired;
        order.exch_timestamp = timestamp;

        self.order_e2l.respond(order);
        Ok(())
    }

    fn partial_fill<const MAKE_RESPONSE: bool>(
        &mut self,
        order: &mut Order,
        timestamp: i64,
        maker: bool,
        exec_price_tick: i64,
        fill_qty: f64,
    ) -> Result<(), BacktestError> {
        if order.status == Status::Expired
            || order.status == Status::Canceled
            || order.status == Status::Filled
        {
            return Err(BacktestError::InvalidOrderStatus);
        }

        // Ensure we don't fill more than available
        let actual_fill_qty = fill_qty.min(order.leaves_qty);

        order.maker = maker;
        if maker {
            order.exec_price_tick = order.price_tick;
        } else {
            order.exec_price_tick = exec_price_tick;
        }

        order.exec_qty = actual_fill_qty;
        order.leaves_qty -= actual_fill_qty;

        // Update status based on remaining quantity
        if order.leaves_qty <= 0.0 {
            order.status = Status::Filled;
        } else {
            order.status = Status::PartiallyFilled;
        }

        order.exch_timestamp = timestamp;

        self.state.apply_fill(order);

        if MAKE_RESPONSE {
            self.order_e2l.respond(order.clone());
        }
        Ok(())
    }

    fn fill_ask_orders_by_crossing(
        &mut self,
        prev_best_tick: i64,
        new_best_tick: i64,
        timestamp: i64,
    ) -> Result<(), BacktestError> {
        let filled = self
            .queue_model
            .on_best_bid_update(prev_best_tick, new_best_tick)?;
        for mut order in filled {
            let price_tick = order.price_tick;
            // For crossing orders, we assume full fill at the order's limit price
            let fill_qty = order.leaves_qty;
            self.partial_fill::<true>(&mut order, timestamp, true, price_tick, fill_qty)?;
        }
        Ok(())
    }
}
