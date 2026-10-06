(ns app.handler
  "HTTP handlers."
  (:require [clojure.tools.logging :as log]
            [clojure.spec.alpha :as s]
            [ring.util.response :refer [response]]))

(s/def ::name string?)

(defn get-user
  "Returns a user."
  [req]
  (if-let [user (find-user (:id req))]
    (response user)
    {:status 404 :body {:message "user not found"}}))

(defn- sync! [conn]
  (try
    (do-sync conn)
    (catch Exception e
      (log/error e "sync failed for" (:id conn))
      (throw (ex-info (format "sync failed: %s" (.getMessage e)) {:conn conn})))))

(defmulti render :type)
(defmethod render :default [x] (str "unknown " (:type x)))
(defprotocol Store (fetch [this id]))
(defrecord MemStore [data])
(def ^:private cache (atom {}))
(deftest get-user-test (is (= 404 (:status (get-user {})))))
