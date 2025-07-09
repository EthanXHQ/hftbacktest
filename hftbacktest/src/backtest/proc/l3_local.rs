use std::collections::{HashMap, hash_map::Entry};

use uuid::timestamp;

use crate::{
    backtest::{
        BacktestError,
        assettype::AssetType,
        models::{FeeModel, LatencyModel},
        order::LocalToExch,
        proc::{LocalProcessor, Processor},
        state::State,
    },
    depth::{L3MarketDepth, L3Order},
    types::{
        AUCTION_UPDATE_EVENT, DEPTH_CLEAR_EVENT, Event, LOCAL_ASK_ADD_ORDER_EVENT,
        LOCAL_ASK_DEPTH_CLEAR_EVENT, LOCAL_BID_ADD_ORDER_EVENT, LOCAL_BID_DEPTH_CLEAR_EVENT,
        LOCAL_CANCEL_ORDER_EVENT, LOCAL_DEPTH_CLEAR_EVENT, LOCAL_EVENT, LOCAL_FILL_EVENT,
        LOCAL_MODIFY_ORDER_EVENT, LOCAL_TRADE_EVENT, OrdType, Order, OrderId, Side, StateValues,
        Status, TimeInForce,
    },
};

/// The Level3 Market-By-Order local model.
pub struct L3Local<AT, LM, MD, FM>
where
    AT: AssetType,
    LM: LatencyModel,
    MD: L3MarketDepth,
    FM: FeeModel,
{
    orders: HashMap<OrderId, Order>,
    order_l2e: LocalToExch<LM>,
    depth: MD,
    state: State<AT, FM>,
    trades: Vec<Event>,
    last_feed_latency: Option<(i64, i64)>,
    last_order_latency: Option<(i64, i64, i64)>,
}

impl<AT, LM, MD, FM> L3Local<AT, LM, MD, FM>
where
    AT: AssetType,
    LM: LatencyModel,
    MD: L3MarketDepth,
    FM: FeeModel,
{
    /// Constructs an instance of `L3Local`.
    pub fn new(
        depth: MD,
        state: State<AT, FM>,
        trade_len: usize,
        order_l2e: LocalToExch<LM>,
    ) -> Self {
        Self {
            orders: Default::default(),
            order_l2e,
            depth,
            state,
            trades: Vec::with_capacity(trade_len),
            last_feed_latency: None,
            last_order_latency: None,
        }
    }
}

impl<AT, LM, MD, FM> LocalProcessor<MD> for L3Local<AT, LM, MD, FM>
where
    AT: AssetType,
    LM: LatencyModel,
    MD: L3MarketDepth,
    FM: FeeModel,
    BacktestError: From<<MD as L3MarketDepth>::Error>,
{
    fn submit_order(
        &mut self,
        order_id: OrderId,
        side: Side,
        price: f64,
        qty: f64,
        order_type: OrdType,
        time_in_force: TimeInForce,
        current_timestamp: i64,
    ) -> Result<(), BacktestError> {
        if self.orders.contains_key(&order_id) {
            return Err(BacktestError::OrderIdExist);
        }

        let price_tick = (price / self.depth.tick_size()).round() as i64;
        let mut order = Order::new(
            order_id,
            price_tick,
            self.depth.tick_size(),
            qty,
            side,
            order_type,
            time_in_force,
        );
        order.req = Status::New;
        order.local_timestamp = current_timestamp;
        self.orders.insert(order.order_id, order.clone());

        self.order_l2e.request(order, |order| {
            order.req = Status::Rejected;
        });

        Ok(())
    }

    fn modify(
        &mut self,
        order_id: OrderId,
        price: f64,
        qty: f64,
        current_timestamp: i64,
    ) -> Result<(), BacktestError> {
        let order = self
            .orders
            .get_mut(&order_id)
            .ok_or(BacktestError::OrderNotFound)?;

        if order.req != Status::None {
            return Err(BacktestError::OrderRequestInProcess);
        }

        let orig_price_tick = order.price_tick;
        let orig_qty = order.qty;

        let price_tick = (price / self.depth.tick_size()).round() as i64;
        order.price_tick = price_tick;
        order.qty = qty;

        order.req = Status::Replaced;
        order.local_timestamp = current_timestamp;

        self.order_l2e.request(order.clone(), |order| {
            order.req = Status::Rejected;
            order.price_tick = orig_price_tick;
            order.qty = orig_qty;
        });

        Ok(())
    }

    fn cancel(&mut self, order_id: OrderId, current_timestamp: i64) -> Result<(), BacktestError> {
        let order = self
            .orders
            .get_mut(&order_id)
            .ok_or(BacktestError::OrderNotFound)?;

        if order.req != Status::None {
            return Err(BacktestError::OrderRequestInProcess);
        }

        order.req = Status::Canceled;
        order.local_timestamp = current_timestamp;

        self.order_l2e.request(order.clone(), |order| {
            order.req = Status::Rejected;
        });

        Ok(())
    }

    fn clear_inactive_orders(&mut self) {
        self.orders.retain(|_, order| {
            order.status != Status::Expired
                && order.status != Status::Filled
                && order.status != Status::Canceled
        })
    }

    fn position(&self) -> f64 {
        self.state_values().position
    }

    fn state_values(&self) -> &StateValues {
        self.state.values()
    }

    fn depth(&self) -> &MD {
        &self.depth
    }

    fn orders(&self) -> &HashMap<OrderId, Order> {
        &self.orders
    }

    fn last_trades(&self) -> &[Event] {
        self.trades.as_slice()
    }

    fn clear_last_trades(&mut self) {
        self.trades.clear();
    }

    fn feed_latency(&self) -> Option<(i64, i64)> {
        self.last_feed_latency
    }

    fn order_latency(&self) -> Option<(i64, i64, i64)> {
        self.last_order_latency
    }
}

impl<AT, LM, MD, FM> Processor for L3Local<AT, LM, MD, FM>
where
    AT: AssetType,
    LM: LatencyModel,
    MD: L3MarketDepth,
    FM: FeeModel,
    BacktestError: From<<MD as L3MarketDepth>::Error>,
{
    fn event_seen_timestamp(&self, event: &Event) -> Option<i64> {
        event.is(LOCAL_EVENT).then_some(event.local_ts)
    }

    fn process(&mut self, ev: &Event) -> Result<(), BacktestError> {
        if !ev.is(AUCTION_UPDATE_EVENT) {
            self.depth.set_allow_price_cross(false);
        } else if ev.is(AUCTION_UPDATE_EVENT) {
            self.depth.set_allow_price_cross(true);
        }

        // Processes a depth event
        if ev.is(LOCAL_BID_DEPTH_CLEAR_EVENT) {
            self.depth.clear_orders(Side::Buy);
        } else if ev.is(LOCAL_ASK_DEPTH_CLEAR_EVENT) {
            self.depth.clear_orders(Side::Sell);
        } else if ev.is(LOCAL_DEPTH_CLEAR_EVENT) {
            self.depth.clear_orders(Side::None);
        } else if ev.is(LOCAL_BID_ADD_ORDER_EVENT) {
            self.depth
                .add_buy_order(ev.order_id, ev.px, ev.qty, ev.local_ts)?;
        } else if ev.is(LOCAL_ASK_ADD_ORDER_EVENT) {
            self.depth
                .add_sell_order(ev.order_id, ev.px, ev.qty, ev.local_ts)?;
        } else if ev.is(LOCAL_MODIFY_ORDER_EVENT) {
            self.depth
                .modify_order(ev.order_id, ev.px, ev.qty, ev.local_ts)?;
        } else if ev.is(LOCAL_CANCEL_ORDER_EVENT) {
            // println!("DELETE {:?}", ev);
            self.depth.delete_order(ev.order_id, ev.local_ts)?;
        } else if !ev.is(AUCTION_UPDATE_EVENT) && ev.is(LOCAL_FILL_EVENT) {
            // println!("FILL {:?}", ev);
            let order1 = self
                .depth
                .orders()
                .get(&ev.order_id)
                .ok_or(BacktestError::OrderNotFound)?;

            // println!("order1 found {:?}", order1);

            let remaining_qty = order1.qty - ev.qty;
            // println!("remaining qty {:?}", remaining_qty);
            // println!("curr price {:?}", order1.price_tick as f64 * self.depth.tick_size());
            self.depth.modify_order(
                ev.order_id,
                order1.price_tick as f64 * self.depth.tick_size(),
                remaining_qty,
                ev.local_ts,
            )?;

            let ival_u64 = ev.ival as u64;
            let order2 = self
                .depth
                .orders()
                .get(&ival_u64)
                .ok_or(BacktestError::OrderNotFound)?;

            // println!("order2 found {:?}", order2);

            let remaining_qty_2 = order2.qty - ev.qty;
            // println!("remaining qty 2 {:?}", remaining_qty_2);
            self.depth.modify_order(
                order2.order_id,
                order2.price_tick as f64 * self.depth.tick_size(),
                remaining_qty_2,
                ev.local_ts,
            )?;

        }
        // Processes a trade event
        else if ev.is(LOCAL_TRADE_EVENT) && self.trades.capacity() > 0 {
            self.trades.push(ev.clone());
        }

        // Stores the current feed latency
        self.last_feed_latency = Some((ev.exch_ts, ev.local_ts));

        Ok(())
    }

    fn process_recv_order(
        &mut self,
        timestamp: i64,
        wait_resp_order_id: Option<OrderId>,
    ) -> Result<bool, BacktestError> {
        // Processes the order part.
        let mut wait_resp_order_received = false;
        while let Some(order) = self.order_l2e.receive(timestamp) {
            // 收到 is_auction order 更新 depth
            // qty < 0 ask 剩余，qty > 0 bid 剩余
            if order.is_auction {
                println!("=============================");
                println!("local auction price: {}", order.exec_price());
                println!("local auction qty: {}", order.qty);

                let auction_price = order.exec_price();
                let auction_price_tick = (auction_price / self.depth.tick_size()).round() as i64;
                let auction_qty = order.qty;
                let timestamp = order.local_timestamp;

                // 1. 全部删除订单
                let mut orders_to_delete = Vec::new();

                for (order_id, order_info) in self.depth.orders() {
                    let should_delete = match order_info.side {
                        Side::Buy => {
                            // 删除高于集合竞价价格的买单, 删除等于集合竞价且数量小于卖单的买单
                            if order_info.price_tick > auction_price_tick {
                                true
                            } else if order_info.price_tick == auction_price_tick
                                && auction_qty > 0.0
                            {
                                true
                            } else {
                                false
                            }
                        }
                        Side::Sell => {
                            // 删除低于集合竞价价格的卖单, 删除等于集合竞价且数量小于买单的卖单
                            if order_info.price_tick < auction_price_tick {
                                true
                            } else if order_info.price_tick == auction_price_tick
                                && auction_qty < 0.0
                            {
                                true
                            } else {
                                false
                            }
                        }
                        _ => false,
                    };

                    if should_delete {
                        orders_to_delete.push(*order_id);
                    }
                }

                // 删除收集到的订单
                for order_id in orders_to_delete {
                    self.depth.delete_order(order_id, timestamp)?;
                }

                // 2. 部分成交订单
                let side = if auction_qty > 0.0 {
                    Side::Sell
                } else {
                    Side::Buy
                };
                let mut at_auction_price: Vec<(OrderId, &L3Order)> = {
                    self.depth
                        .orders()
                        .iter()
                        .filter(|(_order_id, order_info)| {
                            order_info.side == side && order_info.price_tick == auction_price_tick
                        })
                        .map(|(id, order)| (*id, order)) // 保持引用
                        .collect()
                };
                at_auction_price.sort_by_key(|(_order_id, order_info)| order_info.timestamp);

                // println!("at auction {:?}", &at_auction_price);

                let mut total_qty = 0.0;
                for (_, l3order) in &at_auction_price {
                    total_qty += l3order.qty;
                }
                let need_to_fill = total_qty - auction_qty.abs();
                let mut already_filled = 0.0;

                let mut order_to_modify: Option<(OrderId, f64, f64)> = None;
                let mut orders_to_delete = Vec::new();

                for (id, l3order) in at_auction_price {
                    if already_filled >= need_to_fill {
                        break;
                    }
                    // 这个订单需要成交的量
                    let order_fill_qty = (need_to_fill - already_filled).min(order.leaves_qty);
                    already_filled += order_fill_qty;

                    if order_fill_qty >= l3order.qty {
                        orders_to_delete.push(id);
                    } else if order_fill_qty > 0.0 {
                        let remaining_qty = l3order.qty - order_fill_qty;
                        order_to_modify = Some((id, auction_price, remaining_qty));
                    }
                }

                if let Some((id, price, qty)) = order_to_modify {
                    // println!("at auction left {}, {}, {}", id, price, qty);
                    self.depth.modify_order(id, price, qty, timestamp)?;
                }

                for order_id in orders_to_delete {
                    self.depth.delete_order(order_id, timestamp)?;
                }

                // println!("best ask {:?}", self.depth.best_ask());
                // println!("best bid {:?}", self.depth.best_bid());                
            }
            // Updates the order latency only if it has a valid exchange timestamp. When the
            // order is rejected before it reaches the matching engine, it has no exchange
            // timestamp. This situation occurs in crypto exchanges.
            if order.exch_timestamp > 0 {
                self.last_order_latency =
                    Some((order.local_timestamp, order.exch_timestamp, timestamp));
            }

            if let Some(wait_resp_order_id) = wait_resp_order_id {
                if order.order_id == wait_resp_order_id {
                    wait_resp_order_received = true;
                }
            }

            // Processes receiving order response.
            if order.status == Status::Filled {
                self.state.apply_fill(&order);
            }
            // Applies the received order response to the local orders.
            match self.orders.entry(order.order_id) {
                Entry::Occupied(mut entry) => {
                    let local_order = entry.get_mut();
                    if order.req == Status::Rejected {
                        if order.local_timestamp == local_order.local_timestamp {
                            if local_order.req == Status::New {
                                local_order.req = Status::None;
                                local_order.status = Status::Expired;
                            } else {
                                local_order.req = Status::None;
                            }
                        }
                    } else {
                        local_order.update(&order);
                    }
                }
                Entry::Vacant(entry) => {
                    if order.req != Status::Rejected {
                        entry.insert(order);
                    }
                }
            }
        }
        Ok(wait_resp_order_received)
    }

    fn earliest_recv_order_timestamp(&self) -> i64 {
        self.order_l2e
            .earliest_recv_order_timestamp()
            .unwrap_or(i64::MAX)
    }

    fn earliest_send_order_timestamp(&self) -> i64 {
        self.order_l2e
            .earliest_send_order_timestamp()
            .unwrap_or(i64::MAX)
    }
}
