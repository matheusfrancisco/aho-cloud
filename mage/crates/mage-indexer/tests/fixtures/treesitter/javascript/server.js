const express = require("express");
const app = express();

/** Lists users. */
function listUsers(req, res) {
  if (!req.user) {
    return res.status(401).json({ error: "authentication required" });
  }
  res.send(`Hello ${req.user.name}, you have ${count} items`);
}

class UserError extends Error {
  describe() {
    return "user error: " + this.message;
  }
}

app.get("/users", listUsers);

const Greeting = ({ name }) => <div className="greeting">Hello {name}</div>;

module.exports = { listUsers, Greeting };
