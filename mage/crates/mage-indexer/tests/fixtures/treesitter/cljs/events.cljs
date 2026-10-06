(ns webapp.connections.events
  (:require [re-frame.core :as rf]
            [webapp.http :as http]
            [webapp.plugins.core :as plugins]))

(rf/reg-event-fx
 ::fetch
 (fn [{:keys [db]} [_ id]]
   {:db (assoc db :loading true)
    :fx [[:dispatch [::loading id]]
         [:http-xhrio {:uri (str "/api/connections/" id) :method :get}]]}))

(rf/reg-event-db
 :connections/loaded
 (fn [db [_ data]]
   (assoc db :connections data)))

(rf/reg-sub ::list (fn [db] (:connections db)))

(defn enable-plugin! [plugin-name]
  (when-not (plugins/enabled? plugin-name)
    (throw (ex-info (str "Plugin " plugin-name " is not enabled") {})))
  (rf/dispatch [:show-snackbar {:level :error :text "Failed to connect"}])
  (rf/dispatch [::plugins/enable plugin-name])
  (js/console.error "plugin error:" plugin-name)
  @(rf/subscribe [::list]))

(defn panel []
  (let [items @(rf/subscribe [:connections/list])]
    [:div {:class "panel"} (count items) " connections"]))
