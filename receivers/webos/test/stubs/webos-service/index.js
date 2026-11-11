'use strict';

const instances = [];

function Service(busId) {
  this.busId = busId;
  this.handlers = {};
  this.calls = [];
  this.activityManager = {
    create: function (name, cb) {
      setImmediate(function () {
        if (typeof cb === 'function') {
          cb({ name: name });
        }
      });
    },
    complete: function () {},
    adopt: function () {}
  };
  instances.push(this);
}

Service.prototype.register = function (name, callback, cancelCallback) {
  this.handlers[name] = { callback: callback, cancelCallback: cancelCallback };
  return {
    on: function () {}
  };
};

Service.prototype.call = function (uri, params, callback) {
  this.calls.push({ uri: uri, params: params });
  setImmediate(function () {
    if (typeof callback === 'function') {
      callback({ returnValue: true });
    }
  });
};

Service.prototype.subscribe = function (uri, params) {
  return {
    on: function () {},
    cancel: function () {}
  };
};

Service.instances = instances;

Service.lastInstance = function () {
  return instances[instances.length - 1] || null;
};

module.exports = Service;
