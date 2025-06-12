
export enum LoggerType {
    BACKEND,
    FRONTEND,
}

export class Logger {
    private static initialized = false;
    private static log4js = null;
    private static ipcLoggerTags = {
        trace: [],
        debug: [],
        info: [],
        warn: [],
        error: [],
        fatal: [],
    };
    private funcTable = {
        trace: console.trace,
        debug: console.debug,
        info: console.log,
        warn: console.warn,
        error: console.error,
        fatal: console.error,
    };

    public static initialize(config: any) {
        // @ts-ignore
        if (TARGET === 'electron') {
            // @ts-ignore
            const electronAPI = __non_webpack_require__('electron');

            // @ts-ignore
            Logger.log4js = __non_webpack_require__('log4js');
            Logger.log4js.configure(config);
            let logger = Logger.log4js.getLogger();

            electronAPI.ipcMain.on('logger-trace',  (event, value) => {
                if (!Logger.ipcLoggerTags.trace.includes(value.tag)) {
                    Logger.ipcLoggerTags.trace.push(value.tag);
                    logger = Logger.log4js.getLogger(value.tag);
                }

                logger.info(value.message, ...value.optionalParams);
            });

            electronAPI.ipcMain.on('logger-debug',  (event, value) => {
                if (!Logger.ipcLoggerTags.debug.includes(value.tag)) {
                    Logger.ipcLoggerTags.debug.push(value.tag);
                    logger = Logger.log4js.getLogger(value.tag);
                }

                logger.info(value.message, ...value.optionalParams);
            });

            electronAPI.ipcMain.on('logger-info',  (event, value) => {
                if (!Logger.ipcLoggerTags.info.includes(value.tag)) {
                    Logger.ipcLoggerTags.info.push(value.tag);
                    logger = Logger.log4js.getLogger(value.tag);
                }

                logger.info(value.message, ...value.optionalParams);
            });

            electronAPI.ipcMain.on('logger-warn',  (event, value) => {
                if (!Logger.ipcLoggerTags.warn.includes(value.tag)) {
                    Logger.ipcLoggerTags.warn.push(value.tag);
                    logger = Logger.log4js.getLogger(value.tag);
                }

                logger.warn(value.message, ...value.optionalParams);
            });

            electronAPI.ipcMain.on('logger-error',  (event, value) => {
                if (!Logger.ipcLoggerTags.error.includes(value.tag)) {
                    Logger.ipcLoggerTags.error.push(value.tag);
                    logger = Logger.log4js.getLogger(value.tag);
                }

                logger.error(value.message, ...value.optionalParams);
            });

            electronAPI.ipcMain.on('logger-fatal',  (event, value) => {
                if (!Logger.ipcLoggerTags.fatal.includes(value.tag)) {
                    Logger.ipcLoggerTags.fatal.push(value.tag);
                    logger = Logger.log4js.getLogger(value.tag);
                }

                logger.fatal(value.message, ...value.optionalParams);
            });
        }

        Logger.initialized = true;
    }

    constructor(tag: string = 'default', type: LoggerType = LoggerType.FRONTEND) {
        // @ts-ignore
        if (TARGET === 'electron') {
            if (type === LoggerType.BACKEND) {
                // log4js should be initialized via the static method, but providing fallback when logger is used
                // before initialization as done currently by the Store module on startup.
                if (!Logger.log4js) {
                    // @ts-ignore
                    Logger.log4js = __non_webpack_require__('log4js');
                }

                const logger = Logger.log4js.getLogger(tag);
                const initializedFuncTable = {
                    trace: (message?: any, ...optionalParams: any[]) => {
                        logger.trace(message, ...optionalParams);
                    },
                    debug: (message?: any, ...optionalParams: any[]) => {
                        logger.debug(message, ...optionalParams);
                    },
                    info: (message?: any, ...optionalParams: any[]) => {
                        logger.info(message, ...optionalParams);
                    },
                    warn: (message?: any, ...optionalParams: any[]) => {
                        logger.warn(message, ...optionalParams);
                    },
                    error: (message?: any, ...optionalParams: any[]) => {
                        logger.error(message, ...optionalParams);
                    },
                    fatal: (message?: any, ...optionalParams: any[]) => {
                        logger.fatal(message, ...optionalParams);
                    },
                };

                if (Logger.initialized) {
                    this.funcTable = initializedFuncTable;
                }
                else {
                    this.funcTable = {
                        trace: (message?: any, ...optionalParams: any[]) => {
                            if (Logger.initialized) {
                                this.funcTable = initializedFuncTable;
                                this.trace(message, ...optionalParams);
                                return;
                            }

                            console.trace(message, ...optionalParams);
                        },
                        debug: (message?: any, ...optionalParams: any[]) => {
                            if (Logger.initialized) {
                                this.funcTable = initializedFuncTable;
                                this.debug(message, ...optionalParams);
                                return;
                            }

                            console.debug(message, ...optionalParams);
                        },
                        info: (message?: any, ...optionalParams: any[]) => {
                            if (Logger.initialized) {
                                this.funcTable = initializedFuncTable;
                                this.info(message, ...optionalParams);
                                return;
                            }

                            console.info(message, ...optionalParams);
                        },
                        warn: (message?: any, ...optionalParams: any[]) => {
                            if (Logger.initialized) {
                                this.funcTable = initializedFuncTable;
                                this.warn(message, ...optionalParams);
                                return;
                            }

                            console.warn(message, ...optionalParams);
                        },
                        error: (message?: any, ...optionalParams: any[]) => {
                            if (Logger.initialized) {
                                this.funcTable = initializedFuncTable;
                                this.error(message, ...optionalParams);
                                return;
                            }

                            console.error(message, ...optionalParams);
                        },
                        fatal: (message?: any, ...optionalParams: any[]) => {
                            if (Logger.initialized) {
                                this.funcTable = initializedFuncTable;
                                this.fatal(message, ...optionalParams);
                                return;
                            }

                            console.error(message, ...optionalParams);
                        },
                    };
                }

            }
            else if (type === LoggerType.FRONTEND) {
                // @ts-ignore
                const electronAPI = __non_webpack_require__('electron');

                this.funcTable = {
                    trace: (message?: any, ...optionalParams: any[]) => {
                        console.trace(message, ...optionalParams);
                        electronAPI.ipcRenderer.send('logger-trace', { tag: tag, message: message, optionalParams: optionalParams });
                    },
                    debug: (message?: any, ...optionalParams: any[]) => {
                        console.debug(message, ...optionalParams);
                        electronAPI.ipcRenderer.send('logger-debug', { tag: tag, message: message, optionalParams: optionalParams });
                    },
                    info: (message?: any, ...optionalParams: any[]) => {
                        console.log(message, ...optionalParams);
                        electronAPI.ipcRenderer.send('logger-info', { tag: tag, message: message, optionalParams: optionalParams });
                    },
                    warn: (message?: any, ...optionalParams: any[]) => {
                        console.warn(message, ...optionalParams);
                        electronAPI.ipcRenderer.send('logger-warn', { tag: tag, message: message, optionalParams: optionalParams });
                    },
                    error: (message?: any, ...optionalParams: any[]) => {
                        console.error(message, ...optionalParams);
                        electronAPI.ipcRenderer.send('logger-error', { tag: tag, message: message, optionalParams: optionalParams });
                    },
                    fatal: (message?: any, ...optionalParams: any[]) => {
                        console.error(message, ...optionalParams);
                        electronAPI.ipcRenderer.send('logger-fatal', { tag: tag, message: message, optionalParams: optionalParams });
                    },
                };
            }

        // @ts-ignore
        } else if (TARGET === 'webOS' || TARGET === 'tizenOS') {
            this.funcTable = {
                trace: (message?: any, ...optionalParams: any[]) => console.trace(message, ...optionalParams),
                debug: (message?: any, ...optionalParams: any[]) => console.log(message, ...optionalParams),
                info: (message?: any, ...optionalParams: any[]) => console.log(message, ...optionalParams),
                warn: (message?: any, ...optionalParams: any[]) => console.warn(message, ...optionalParams),
                error: (message?: any, ...optionalParams: any[]) => console.error(message, ...optionalParams),
                fatal: (message?: any, ...optionalParams: any[]) => console.error(message, ...optionalParams),
            };
        } else {
            // @ts-ignore
            console.warn(`Attempting to initialize logger on unsupported target: ${TARGET}`);
        }
    }

    public trace(message?: any, ...optionalParams: any[]): void {
        this.funcTable.trace(message, ...optionalParams);
    }

    public debug(message?: any, ...optionalParams: any[]): void {
        this.funcTable.debug(message, ...optionalParams);
    }

    public info(message?: any, ...optionalParams: any[]): void {
        this.funcTable.info(message, ...optionalParams);
    }

    public warn(message?: any, ...optionalParams: any[]): void {
        this.funcTable.warn(message, ...optionalParams);
    }

    public error(message?: any, ...optionalParams: any[]): void {
        this.funcTable.error(message, ...optionalParams);
    }

    public fatal(message?: any, ...optionalParams: any[]): void {
        this.funcTable.fatal(message, ...optionalParams);
    }

    public shutdown() {
        // @ts-ignore
        if (TARGET === 'electron') {
            Logger.log4js.shutdown();
        }
    }
}
