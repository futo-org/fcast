package com.futo.fcast.receiver.proxy.server

import com.futo.fcast.receiver.proxy.ManagedHttpClient
import com.futo.fcast.receiver.proxy.server.exceptions.EmptyRequestException
import com.futo.fcast.receiver.proxy.server.handlers.HttpFunctionHandler
import com.futo.fcast.receiver.proxy.server.handlers.HttpHandler
import com.futo.fcast.receiver.proxy.server.handlers.HttpOptionsAllowHandler
import com.futo.fcast.receiver.proxy.Logger
import java.io.BufferedInputStream
import java.io.OutputStream
import java.lang.reflect.Field
import java.lang.reflect.Method
import java.net.InetAddress
import java.net.NetworkInterface
import java.net.ServerSocket
import java.net.Socket
import java.util.UUID
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.stream.IntStream.range

class ManagedHttpServer(private val _requestedPort: Int = 0) {
    private val _client : ManagedHttpClient = ManagedHttpClient();
    private val _logVerbose: Boolean = false;

    var active : Boolean = false
        private set;
    private var _stopCount = 0;
    var port = 0
            private set;

    private val _handlers = hashMapOf<String, HashMap<String, HttpHandler>>()
    private val _headHandlers = hashMapOf<String, HttpHandler>()
    private var _workerPool: ExecutorService? = null;

    @Synchronized
    fun start() {
        if (active)
            return;
        active = true;
        _workerPool = Executors.newCachedThreadPool();

        Thread {
            try {
                val socket = ServerSocket(_requestedPort);
                port = socket.localPort;

                val stopCount = _stopCount;
                while (_stopCount == stopCount) {
                    if(_logVerbose)
                        Logger.i(TAG, "Waiting for connection...");
                    val s = socket.accept() ?: continue;

                    try {
                        handleClientRequest(s);
                    }
                    catch(ex : Exception) {
                        Logger.e(TAG, "Client disconnected due to: " + ex.message, ex);
                    }
                }
            } catch (e: Throwable) {
                Logger.e(TAG, "Failed to accept socket.", e);
                stop();
            }
        }.start();

        Logger.i(TAG, "Started HTTP Server ${port}. \n" + getAddresses().map { it.hostAddress }.joinToString("\n"));
    }
    @Synchronized
    fun stop() {
        _stopCount++;
        active = false;
        _workerPool?.shutdown();
        _workerPool = null;
        port = 0;
    }

    private fun handleClientRequest(socket: Socket) {
        _workerPool?.submit {
            val requestStream = BufferedInputStream(socket.getInputStream());
            val responseStream = socket.getOutputStream();

            val requestId = UUID.randomUUID().toString().substring(0, 5);
            try {
                keepAliveLoop(requestStream, responseStream, requestId) { req ->
                    req.use { httpContext ->
                        if(!httpContext.path.startsWith("/plugin/"))
                            Logger.i(TAG, "[${req.id}] ${httpContext.method}: ${httpContext.path}")
                        else
                            ;//Logger.v(TAG, "[${req.id}] ${httpContext.method}: ${httpContext.path}")
                        val handler = getHandler(httpContext.method, httpContext.path);
                        if (handler != null) {
                            handler.handle(httpContext);
                        } else {
                            Logger.i(TAG, "[${req.id}] 404 on ${httpContext.method}: ${httpContext.path}");
                            httpContext.respondCode(404);
                        }
                        if(_logVerbose)
                            Logger.i(TAG, "[${req.id}] Responded [${req.statusCode}] ${httpContext.method}: ${httpContext.path}")
                    };
                }
            }
            catch(emptyRequest: EmptyRequestException) {
                if(_logVerbose)
                    Logger.i(TAG, "[${requestId}] Request ended due to empty request: ${emptyRequest.message}");
            }
            catch (e: Throwable) {
                Logger.e(TAG, "Failed to handle client request.", e);
            }
            finally {
                requestStream.close();
                responseStream.close();
            }
        };
    }

    fun getHandler(method: String, path: String) : HttpHandler? {
        synchronized(_handlers) {
            if (method == "HEAD") {
                return _headHandlers[path]
            }

            val handlerMap = _handlers[method] ?: return null
            return handlerMap[path]
        }
    }
    fun addHandler(handler: HttpHandler, withHEAD: Boolean = false) : HttpHandler {
        synchronized(_handlers) {
            handler.allowHEAD = withHEAD;

            var handlerMap: HashMap<String, HttpHandler>? = _handlers[handler.method];
            if (handlerMap == null) {
                handlerMap = hashMapOf()
                _handlers[handler.method] = handlerMap
            }

            handlerMap[handler.path] = handler;
            if (handler.allowHEAD || handler.method == "HEAD") {
                _headHandlers[handler.path] = handler
            }
        }
        return handler;
    }

    fun addHandlerWithAllowAllOptions(handler: HttpHandler, withHEAD: Boolean = false) : HttpHandler {
        val allowedMethods = arrayListOf(handler.method, "OPTIONS")
        if (withHEAD) {
            allowedMethods.add("HEAD")
        }

        val tag = handler.tag
        if (tag != null) {
            addHandler(HttpOptionsAllowHandler(handler.path, allowedMethods).withTag(tag))
        } else {
            addHandler(HttpOptionsAllowHandler(handler.path, allowedMethods))
        }

        return addHandler(handler, withHEAD)
    }

    fun removeHandler(method: String, path: String) {
        synchronized(_handlers) {
            val handlerMap = _handlers[method] ?: return
            val handler = handlerMap.remove(path) ?: return
            if (method == "HEAD" || handler.allowHEAD) {
                _headHandlers.remove(path)
            }
        }
    }
    fun removeAllHandlers(tag: String? = null) {
        synchronized(_handlers) {
            if(tag == null)
                _handlers.clear();
            else {
                for (pair in _handlers) {
                    val toRemove = ArrayList<String>()
                    for (innerPair in pair.value) {
                        if (innerPair.value.tag == tag) {
                            toRemove.add(innerPair.key)

                            if (pair.key == "HEAD" || innerPair.value.allowHEAD) {
                                _headHandlers.remove(innerPair.key)
                            }
                        }
                    }

                    for (x in toRemove)
                        pair.value.remove(x)
                }
            }
        }
    }
    fun addBridgeHandlers(obj: Any, tag: String? = null) {
        //val tagToUse = tag ?: obj.javaClass.name;
        val getMethods = obj::class.java.declaredMethods
            .filter { it.getAnnotation(HttpGET::class.java) != null }
            .map { Pair<Method, HttpGET>(it, it.getAnnotation(HttpGET::class.java)!!) }
            .toList();
        val postMethods = obj::class.java.declaredMethods
            .filter { it.getAnnotation(HttpPOST::class.java) != null }
            .map { Pair<Method, HttpPOST>(it, it.getAnnotation(HttpPOST::class.java)!!) }
            .toList();

        val getFields = obj::class.java.declaredFields
            .filter { it.getAnnotation(HttpGET::class.java) != null && it.type == String::class.java }
            .map { Pair<Field, HttpGET>(it, it.getAnnotation(HttpGET::class.java)!!) }
            .toList();

        for(getMethod in getMethods)
            if(getMethod.first.parameterTypes.firstOrNull() == HttpContext::class.java && getMethod.first.parameterCount == 1)
                addHandler(HttpFunctionHandler("GET", getMethod.second.path) { getMethod.first.invoke(obj, it) }).apply {
                    if(!getMethod.second.contentType.isEmpty())
                        this.withContentType(getMethod.second.contentType);
                }.withContentType(getMethod.second.contentType);
        for(postMethod in postMethods)
            if(postMethod.first.parameterTypes.firstOrNull() == HttpContext::class.java && postMethod.first.parameterCount == 1)
                addHandler(HttpFunctionHandler("POST", postMethod.second.path) { postMethod.first.invoke(obj, it) }).apply {
                    if(!postMethod.second.contentType.isEmpty())
                        this.withContentType(postMethod.second.contentType);
                }.withContentType(postMethod.second.contentType);

        for(getField in getFields) {
            getField.first.isAccessible = true;
            addHandler(HttpFunctionHandler("GET", getField.second.path) {
                val value = getField.first.get(obj) as String?;
                if(value != null) {
                    val headers = HttpHeaders(
                        Pair("Content-Type", getField.second.contentType)
                    );
                    it.respondCode(200, headers, value);
                }
                else
                    it.respondCode(204);
            }).withContentType(getField.second.contentType);
        }
    }

    private fun keepAliveLoop(requestReader: BufferedInputStream, responseStream: OutputStream, requestId: String, handler: (HttpContext)->Unit) {
        val stopCount = _stopCount;
        var keepAlive: Boolean;
        var requestsMax = 0;
        var requestsTotal = 0;
        do {
            val req = HttpContext(requestReader, responseStream, requestId);

            //Handle Request
            handler(req);

            requestsTotal++;
            if(req.keepAlive) {
                keepAlive = true;
                if(req.keepAliveMax > 0)
                    requestsMax = req.keepAliveMax;

                req.skipBody();
            } else {
                keepAlive = false;
            }
        }
        while (keepAlive && (requestsMax == 0 || requestsTotal < requestsMax) && _stopCount == stopCount);
    }

    fun getAddressByIP(addresses: List<InetAddress>) : String = getAddress(addresses.map { it.address }.toList());
    fun getAddress(addresses: List<ByteArray> = listOf()): String {
        if(addresses.isEmpty())
            return getAddresses().first().hostAddress ?: "";
        else
            //Matches the closest address to the list of provided addresses
            return getAddresses().maxBy {
                val availableAddress = it.address;
                return@maxBy addresses.map { deviceAddress ->
                    var matches = 0;
                    for(index in range(0, Math.min(availableAddress.size, deviceAddress.size))) {
                        if(availableAddress[index] == deviceAddress[index])
                            matches++;
                        else
                            break;
                    }
                    return@map matches;
                }.max();
            }.hostAddress ?: "";
    }
    private fun getAddresses(): List<InetAddress> {
        val addresses = arrayListOf<InetAddress>();

        try {
            for (intf in NetworkInterface.getNetworkInterfaces()) {
                for (addr in intf.inetAddresses) {
                    if (!addr.isLoopbackAddress) {
                        val ipString: String = addr.hostAddress ?: continue
                        val isIPv4 = ipString.indexOf(':') < 0
                        if (!isIPv4) {
                            continue
                        }

                        addresses.add(addr)
                    }
                }
            }
        }
        catch (ignored: Exception) { }

        return addresses;
    }

    companion object {
        val TAG = "ManagedHttpServer";
    }
}